//! One machine's connection and everything that hangs on it: signing in,
//! starting the helper, starting the runtime or attaching to it, getting the
//! connection back when it drops, and the one loopback port (`Listener`) that
//! stays the same through all of it. A caller says what it wants (`Want`) and
//! hears how it stands (`Outcome`); the work goes on in a thread of the
//! `Session`, which ends when the session is closed or dropped. Closing
//! detaches from the helper and never stops the runtime.

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

/// How long a reconnect waits after its first failure, doubling up to `RETRY_LAST`.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LAST: Duration = Duration::from_secs(30);

/// A connection that stays lost this long is given up on (state `failed`).
const RETRY_GIVE_UP: Duration = Duration::from_secs(10 * 60);

/// How often `reattach` asks the helper whether the runtime is there, and how long it waits between.
const REATTACH_TRIES: u32 = 3;
const REATTACH_PAUSE: Duration = Duration::from_secs(1);

/// Where a session stands, with what each state knows.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Signing in and starting the helper, or getting the connection back.
    Connecting,
    /// The helper is up and no runtime is asked for (or it was stopped).
    Connected,
    /// The helper is up and no runtime runs: an attach (`Want::Attach`) found none, or the one
    /// attached ended. Nothing was started.
    NothingRunning,
    /// A runtime is starting or being attached to. On a cluster, `queue` is Slurm's word once the
    /// job runs: its state `RUNNING`, and the node as its reason.
    Starting { queue: Option<QueueInfo> },
    /// A cluster job waits in the queue.
    Queued(QueueInfo),
    /// The runtime is attached and answers through the listener's port.
    Ready(RuntimeInfo),
    /// Endeavor must install something on the machine first and the user hasn't agreed. Not a
    /// failure, and not tried again by itself: asking again with `install`, or
    /// `Session::allow_install` for the helper, goes on.
    NeedsInstall(InstallInfo),
    /// The last step didn't work, in plain words. Asking again tries again.
    Failed(String),
}

impl State {
    /// The runtime, once it is attached.
    pub fn runtime(&self) -> Option<&RuntimeInfo> {
        if let State::Ready(runtime) = self { Some(runtime) } else { None }
    }

    /// Slurm's state and reason for the job, while it waits or Julia starts in it.
    pub fn queue(&self) -> Option<&QueueInfo> {
        match self {
            State::Queued(queue) | State::Starting { queue: Some(queue) } => Some(queue),
            _ => None,
        }
    }

    /// Why it failed.
    pub fn error(&self) -> Option<&str> {
        if let State::Failed(why) = self { Some(why) } else { None }
    }
}

/// What Endeavor wants to install on the machine, and what it found there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstallInfo {
    /// What is needed, in the order it would be installed.
    pub items: Vec<wire::Item>,
    /// Set exactly when Endeavor's helper is one of the items: the platform, where it
    /// would go, its size and what runs there already.
    pub helper: Option<NeedsInstall>,
}

impl InstallInfo {
    /// The helper is missing: the one item, with what was found on the machine.
    pub(crate) fn helper(found: NeedsInstall) -> InstallInfo {
        let item = wire::Item { kind: wire::KIND_HELPER.into(), name: "Endeavor's helper".into(), size_mb: found.bytes.map(|bytes| bytes.div_ceil(1_000_000).max(1)), place: Some(found.folder.clone()) };
        InstallInfo { items: vec![item], helper: Some(found) }
    }

    /// Whether the helper is among them.
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
    /// How the helper runs the runtime on this connection: "process" or "slurm"; None from a helper that doesn't say.
    pub launcher: Option<String>,
    /// `uname`'s words for the machine, as `linux` and `x86_64`.
    pub os: Option<String>,
    pub arch: Option<String>,
    /// This connect installed the helper there, or it was already installed.
    pub helper_installed: Option<bool>,
    /// What the helper found to start the runtime with (Julia), once it has started one.
    pub found: Vec<FoundInfo>,
    /// Slurm's partitions, on a machine that has Slurm.
    pub partitions: Option<Vec<Partition>>,
    /// Asking Slurm for them failed, so `partitions` is empty.
    #[serde(default)]
    pub partitions_failed: bool,
    pub scratch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FoundInfo {
    /// "Julia".
    pub name: String,
    pub version: String,
    pub path: String,
}

/// How to reach the runtime that is attached.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// The listener's port on this computer. It stays the same while the session lives.
    pub port: u16,
    /// For `Authorization: Bearer` on every request to that port.
    pub token: String,
    pub mcp_url: String,
    /// Pluto's start page, with the token that lets a browser in.
    pub page_url: String,
    pub node: String,
    pub pid: u32,
    /// It was running already; this session didn't start it.
    pub reattached: bool,
    pub job: Option<wire::slurm::Job>,
    /// The runtime's own port on `node`, where a server's user can forward it; none when an older helper doesn't say.
    pub remote_port: Option<u16>,
    /// The build that started it, as its record says; none when the record or an older helper doesn't say.
    #[serde(default)]
    pub build: Option<String>,
    /// The number for what its core offers callers (`CORE_INTERFACE`), as its record says; none when the
    /// record or an older helper doesn't say.
    #[serde(default)]
    pub interface: Option<u32>,
}

impl RuntimeInfo {
    /// This build's tools and calls work with it as it is: its core offers this build's interface
    /// (`CORE_INTERFACE`), or this build started it. False when neither is known.
    pub fn usable_as_is(&self) -> bool {
        crate::usable_as_is(self.build.as_deref(), self.interface)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobInfo {
    pub id: String,
    /// "8 CPUs · 32 GB · 8 h".
    pub summary: Option<String>,
    pub node: Option<String>,
    /// When Slurm will end it (Unix seconds).
    pub ends_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueInfo {
    pub state: String,
    pub reason: String,
}

/// Everything a session knows about its machine, read without changing anything.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    /// The machine's id in the machines file.
    pub machine: String,
    /// The name the agent knows it by.
    pub name: String,
    pub state: State,
    /// The last progress line.
    pub step: Option<String>,
    /// What the helper reported, once it is connected.
    pub hello: Option<HelloInfo>,
    /// The cluster job, from the time it is submitted. It outlasts a lost connection, since the job goes on.
    pub job: Option<JobInfo>,
}

/// What a caller wants of the machine's runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    /// Attach to a runtime that runs (on a cluster, a job that waits or runs) and start nothing
    /// otherwise. `install` is the user's agreement to the helper on the machine, if it lacks it.
    Attach { install: bool },
    /// Start the runtime, or attach to the one running. `job` is what to submit on a cluster; on a
    /// session that already has a runtime or a queued job it is not used, and the outcome is that
    /// of what is there (a cluster has one job for each user, and the outcome says its size).
    /// `install` is the user's agreement to what is missing on the machine: the helper, and for
    /// this start only, whatever the helper finds the start needs (such as Julia).
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

/// How it stands with what was wanted.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The runtime is attached and answers on `RuntimeInfo::port`.
    Ready(RuntimeInfo),
    /// A cluster job waits in the queue.
    Queued { job: Option<JobInfo>, queue: QueueInfo },
    /// An attach found no runtime, and nothing was started.
    NothingRunning,
    /// Something must be installed first, and the user hasn't agreed.
    NeedsInstall(InstallInfo),
    /// It didn't work, in plain words.
    Failed(String),
    /// Not settled when the wait ran out: the step it is at.
    StillWorking(String),
}

/// The helper binary to send to a machine whose `uname -s` is `os` and `uname -m` is `arch` (`Options::helper`).
pub type HelperFor = Box<dyn Fn(&str, &str) -> Result<PathBuf, String> + Send + Sync>;

/// What a session tells its caller as it goes (`Config::on_event`). It comes on a thread of the
/// session's, after the session has recorded it and with no lock of the session held, so the
/// caller may ask the session for its status and sees the state the event brought. The callback
/// should return quickly, and must not close or drop the session: it runs on the session's own
/// threads. Not every change of state comes with an event (a runtime that is ready, or found not
/// running, doesn't), so a caller that shows the state still reads `Session::status`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// A step of connecting or starting, as the helper reported it.
    Step(Event),
    /// Something that went wrong or waits on the user, in plain words: a lost connection, a start
    /// that failed or needs an install, a wait for the agreement to install the helper, a runtime
    /// that stopped or that another connection took over.
    Trouble(String),
}

/// Where a session's events go (`Config::on_event`).
pub type OnEvent = Box<dyn Fn(SessionEvent) + Send + Sync>;

/// How a session reaches its machine.
pub struct Config {
    pub server: Server,
    pub transport: Transport,
    /// Where the helper is installed (`Options::root`); empty is the default.
    pub root: String,
    /// The runtime's state folder on the machine (`Options::state`); empty is the default.
    pub state: String,
    /// Julia's depot on the machine (`Options::depot`); empty is the default.
    pub depot: String,
    /// The user agreed to installing the helper from the start. Without it a machine that lacks it
    /// waits for `Session::allow_install` or a `Want` with `install`.
    pub allow_install: bool,
    /// What the listener tells an agent whose runtime is away.
    pub messages: Messages,
    pub helper: HelperFor,
    /// How the helper runs the runtime (`Options::launcher`); None is the record's way.
    pub launcher: Option<Launcher>,
    /// How ssh signs in (`Options::auth`): `Auth::Batch` by default. A caller that answers ssh's
    /// prompts itself passes `Auth::Env` with its askpass.
    pub auth: Auth,
    /// A runtime this session starts ends itself once no notebook has been open for the idle limit
    /// (`Options::exit_idle`); true by default.
    pub exit_idle: bool,
    /// Hears the session's events. The default writes each `Trouble` to stderr and drops the steps.
    pub on_event: OnEvent,
}

impl Config {
    /// Over `ssh` to `server`, with the machine's default folders. `helper` finds the helper to send to a machine that lacks it.
    pub fn new(server: Server, helper: impl Fn(&str, &str) -> Result<PathBuf, String> + Send + Sync + 'static) -> Config {
        let transport = Transport::for_server(&server);
        let on_event: OnEvent = Box::new(|event| {
            if let SessionEvent::Trouble(text) = event {
                eprintln!("{text}");
            }
        });
        Config { server, transport, root: String::new(), state: String::new(), depot: String::new(), allow_install: false, messages: Messages::default(), helper: Box::new(helper), launcher: None, auth: Auth::Batch, exit_idle: true, on_event }
    }
}

/// What the supervisor is told.
enum Msg {
    /// A start request while it waits to connect, or after a failure.
    Kick,
    /// The session is closing.
    Quit,
    /// The helper of this connection ended by itself.
    Closed(u64),
    /// A runtime start on this connection ended (`Inner::epoch` when it began, and whether it was allowed to install).
    Started(u64, u64, bool, Box<Result<Runtime, StartError>>),
    /// The attached runtime went away.
    Notice(u64, Notice),
}

/// What a start asked for.
#[derive(Clone, Debug, Default)]
struct Wish {
    /// On a cluster, what to submit.
    job: Option<JobRequest>,
    /// Only attach: first ask the helper whether a runtime is there, and start nothing if not.
    attach: bool,
    /// The helper may install what this start needs: the `install` of the
    /// request, and not what was agreed for an earlier one. Never for a start
    /// that only attaches.
    install: bool,
}

/// The start the session counts as under way. It is not the state: a start goes on while the state is
/// `Failed` (a stop it refused, or a reattach that found the old runtime gone as a new start began), and
/// one the runtime's end cut short must not end `Ready`.
#[derive(PartialEq)]
enum Run {
    /// No start is under way.
    Idle,
    /// A start is under way.
    Starting,
    /// The runtime went away before the end of its start was handled: that end is not a success.
    Gone,
}

impl Inner {
    fn starting(&self) -> bool {
        matches!(self.run, Run::Starting | Run::Gone)
    }

    /// A runtime is attached or a start is under way: nothing new is begun on the connection.
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
    run: Run,
    /// Counts the stops, so the end of a start that one cut short is ignored.
    /// A stop counts before it asks the helper, which takes a while.
    epoch: u64,
    /// The supervisor waits for a `Kick` or for the end of a pause, and none is in its inbox.
    may_kick: bool,
    /// What the first helper settled `Launcher::Auto` as: a reconnect asks for the same, so the
    /// session never changes how it runs the runtime (if Slurm was installed meanwhile, say).
    settled: Option<Launcher>,
    /// The runtime (its pid) the caller was told came from another build: a reconnect to it says nothing again.
    told_other_build: Option<u32>,
}

struct Shared {
    config: Config,
    inbox: Sender<Msg>,
    listener: Arc<Listener>,
    inner: Mutex<Inner>,
    /// Told after every change of `inner`.
    changed: Condvar,
    /// The connect under way, which a close cancels.
    cancel: Mutex<Arc<Cancel>>,
    leaving: AtomicBool,
    /// The user agreed to installing the helper on the machine (`Config::allow_install`,
    /// `Session::allow_install`, or a `Want` with `install` while there is no connection). Kept
    /// for as long as the session lives, so that a reconnect to a machine that lost the helper
    /// doesn't need it again. Julia's download is not part of it: it is asked for by each start
    /// (`Wish::install`).
    allow_install: AtomicBool,
}

/// One machine's connection. Closing it, or dropping it, detaches from the machine's helper
/// and ends its thread; the runtime goes on.
pub struct Session {
    shared: Arc<Shared>,
    supervisor: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Session {
    /// Open the listener and begin connecting to the machine in a thread of its own; nothing is
    /// asked of the runtime until `ensure`.
    pub fn new(config: Config) -> Result<Session, String> {
        let name = config.server.display_name();
        let listener = Listener::new(&name, None, config.messages)?;
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

    /// Ask for `want` and say how it stands, as soon as that is known or `wait` has passed
    /// (`Outcome::StillWorking`). The work goes on meanwhile.
    ///
    /// What is asked for is judged from what the session holds, not from who asked: with a start or
    /// an attach under way, or a runtime attached, the call only waits for it, so that any number of
    /// callers can ask at once. `install: true` is an agreement and upgrades a start under way that
    /// lacked it, and a start that ended `NeedsInstall` begins again with it. An `Attach` never
    /// replaces a `Start` that is wanted or under way, and a `Start` is never answered
    /// `NothingRunning`. A `Start` with a job on a session that already has a runtime or a queued job
    /// gets what is there. A failure, and a need to install without `install`, is kept and given to every
    /// call until one asks to `retry`, which begins again.
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

    /// Ask for `want` as `ensure` does and return at once; `status` has the rest.
    pub fn request(&self, want: &Want) {
        self.shared.request_start(want);
    }

    /// Where the session stands.
    pub fn status(&self) -> Status {
        status_of(&self.shared.inner())
    }

    /// Where the session stands once `done` is true of it, or after `wait` if it never is.
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

    /// Whether the runtime runs in Slurm jobs through this session: as the helper said, else as the
    /// session was made. A launcher of `Auto` the helper hasn't settled yet counts as Slurm, so that
    /// nothing takes it for a machine where a start runs Julia directly.
    pub fn cluster(&self) -> bool {
        let settled = self.shared.inner().settled;
        settled.or(self.shared.config.launcher).unwrap_or_else(|| self.shared.config.server.launcher()) != Launcher::Process
    }

    /// Ask the machine's helper about its files (`Request`), once the session has connected or
    /// `wait` has passed. None when it isn't connected by then: it is still connecting, the helper
    /// isn't installed, or the connection failed (`status` says which).
    pub fn files(&self, request: Request, wait: Duration) -> Option<Result<Reply, String>> {
        self.wait_for(wait, |status| status.state != State::Connecting);
        let channel = self.shared.inner().channel.clone()?;
        Some(channel.files(request))
    }

    /// Whether the helper is connected, which `stop` needs.
    pub fn connected(&self) -> bool {
        self.shared.inner().channel.is_some()
    }

    /// Stop the runtime, for every client of it, and wait until it is gone (up to a minute and a
    /// bit). The connection stays, and the error says why when the runtime didn't stop.
    pub fn stop(&self) -> Result<(), String> {
        self.shared.request_stop(false)
    }

    /// `stop`, but a start under way that another connection began is cancelled too (`Channel::force_stop`).
    pub fn force_stop(&self) -> Result<(), String> {
        self.shared.request_stop(true)
    }

    /// Stop the runtime and start it again (`job` and `install` as in `Want::Start`), then answer as
    /// `ensure` does (a kept failure is tried again). It is the stop of `stop`: Julia ends for every
    /// client of it, the plugin's too, whether or not it is busy, and the notebooks that were open are
    /// not opened again in the new runtime. Calls to the listener's port meanwhile are told that Julia
    /// is restarting, and, if the start fails, that it couldn't start (`Messages::restart_failed`),
    /// rather than that the machine isn't connected. A stop that fails starts nothing and is the outcome.
    pub fn restart(&self, job: Option<JobRequest>, install: bool, wait: Duration) -> Outcome {
        if let Err(message) = self.shared.request_stop(false) {
            return Outcome::Failed(message);
        }
        // After the stop, which says the machine isn't connected, and before the start can attach.
        self.shared.listener.restarting();
        self.ensure(Want::Start { job, install }, wait, true)
    }

    /// The user agreed to install the helper on the machine, and nothing a start needs after it:
    /// a session waiting for that connects again with it allowed. Returns at once.
    pub fn allow_install(&self) {
        self.shared.request_install();
    }

    /// The listener's port on this computer, the same for the session's whole life.
    pub fn port(&self) -> u16 {
        self.shared.listener.port()
    }

    /// Let go of the machine's helper (which leaves the runtime running, and discards what an
    /// upload had written part way), end the connection, close the listener and end the session's
    /// thread. A connect that is under way is cancelled at once. Nothing begins after this, and calls
    /// answer `Outcome::Failed`.
    pub fn close(&self) {
        let shared = &self.shared;
        if shared.leaving.swap(true, Ordering::SeqCst) {
            return;
        }
        // The helper is told before ssh is ended, or it would only see its input end.
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
        // A connection that came while this waited.
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
    }
}

/// Tell the helper to let go, and wait a short time for it: a connection that is dead doesn't hold anything up.
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

/// How it stands, if that is settled.
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

    /// Tell the caller something went wrong or waits on the user.
    fn trouble(&self, text: String) {
        (self.config.on_event)(SessionEvent::Trouble(text));
    }

    /// Hear what connecting and starting say, and pass it on.
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
                // A job this session hasn't heard submitted, such as one it re-attached to, is known from here.
                if i.job.as_ref().is_none_or(|known| known.id != job) {
                    i.job = Some(JobInfo { id: job, ..Default::default() });
                }
                // The helper reports the node, as `reason`, once the job runs.
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

    /// Start the runtime on the connection if one is asked for and none is under
    /// way or attached. Never called with the lock held.
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

    /// For a start that only attaches: ask the helper whether a runtime runs or a job waits, and
    /// give the wish to go on with. If not, end the start with nothing started, unless the wish
    /// became a start meanwhile. None then, and when the answer was no use.
    fn runtime_is_there(&self, channel: &Channel, conn: u64, epoch: u64) -> Option<Wish> {
        let answer = channel.files(Request::Runtime);
        // A stop or a close that came meanwhile has ended this start: nothing is attached to what it stopped.
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
            // The connection went: `Closed` follows and takes the wish along to the next one.
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

    /// The helper may be installed. A machine that lacks it has no connection, and is connected again.
    fn request_install(&self) {
        self.allow_install.store(true, Ordering::SeqCst);
        self.with(|i| {
            if matches!(i.state, State::NeedsInstall(_)) && i.channel.is_none() {
                self.reconnect_now(i);
            }
        });
    }

    fn reconnect_now(&self, i: &mut Inner) {
        i.state = State::Connecting;
        self.kick(i);
    }

    /// Wake the supervisor if it waits. One wake-up is enough, and none while it connects.
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

    /// Record what `want` asks for, under the lock. True when the runtime should be started on the
    /// connection that is there (`begin_start`, called with no lock held).
    fn ask(&self, i: &mut Inner, want: &Want) -> bool {
        // Checked under the lock, as a close that came meanwhile has ended the supervisor and taken the connection.
        if self.leaving.load(Ordering::SeqCst) {
            return false;
        }
        // Only a start with no connection can mean the helper: with one, `install` is for what the start needs, and a
        // yes to that mustn't be kept as one for the helper. Set under the lock, as the supervisor checks it.
        if want.install() && i.channel.is_none() {
            self.allow_install.store(true, Ordering::SeqCst);
        }
        // What is wished, or held, is not asked for again: that would restart a start, or cut a pause between attempts short.
        let wished = i.wanted.is_some() || i.resume.is_some();
        if i.busy() || (wished && !matches!(i.state, State::Failed(_) | State::NeedsInstall(_))) {
            if let Some(wish) = i.wanted.as_mut().or(i.resume.as_mut()) {
                match want {
                    // A start replaces an attach that has not found out yet, and never the other way round.
                    Want::Start { job, install } if wish.attach => *wish = Wish { job: job.clone(), attach: false, install: *install },
                    _ => wish.install |= want.wish().install,
                }
            }
            return false;
        }
        i.wanted = Some(want.wish());
        i.resume = None;
        if i.state == State::NothingRunning {
            i.state = State::Connected;
        }
        if i.channel.is_some() {
            return true;
        }
        // No connection: the supervisor starts the runtime once it has one, and tries now if it was waiting.
        if matches!(i.state, State::Failed(_) | State::NeedsInstall(_)) {
            i.state = State::Connecting;
        }
        self.kick(i);
        false
    }

    fn request_stop(&self, force: bool) -> Result<(), String> {
        // Counted before the helper is asked: it takes a while, and the end of a start it cuts short is no failure.
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
                    // A connection that comes later mustn't attach to what was stopped.
                    i.resume = None;
                    if same {
                        i.wanted = None;
                        i.run = Run::Idle;
                        i.job = None;
                        i.state = State::Connected;
                        i.step = Some("The runtime was stopped".into());
                    }
                }
                Err(message) => {
                    // The runtime is still there, and the start that was under way goes on.
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

/// Why a stop can't reach the helper.
fn not_connected_to_stop(i: &Inner) -> String {
    let name = &i.name;
    match &i.state {
        State::NeedsInstall(_) => format!("Endeavor isn't connected to {name}: its helper isn't installed there, and stopping the runtime needs it. Installing it wasn't agreed to."),
        State::Failed(why) => format!("Endeavor isn't connected to {name}: {why}"),
        _ => format!("Endeavor isn't connected to {name} yet, so it can't stop the runtime. It is still connecting."),
    }
}

/// The connection and what hangs on it, until the session is closed: connects, starts what was asked for, and gets it back.
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
        // Wake-ups from before this connect are answered by it.
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
                    // The permission may have come while this connect looked.
                    if shared.allow_install.load(Ordering::SeqCst) {
                        continue;
                    }
                    let parked = shared.with(|i| {
                        // Checked with the lock held, so that a permission is never lost between the two.
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
                    // The listener has been saying that the connection comes back by itself.
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
                    // A start asked for while the connection was away is the newer wish.
                    i.resume.take().filter(|_| i.wanted.is_none())
                });
                // A close that came during the connect detaches it, and nothing is started on it.
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
/// helper says it is still there (running, starting, or a job that waits): a start would
/// otherwise begin a new runtime, or on a cluster submit a job nobody asked for.
/// An answer that isn't clear is asked for again, and then leaves the session `Failed`.
fn reattach(shared: &Shared, channel: &Channel, wish: Wish) {
    let mut trouble = String::new();
    // A stop or a close during the asking has ended what is brought back.
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
        // The connection went again: the next one tries, with the same wish.
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

/// Connect to the machine with the record the session was made with.
fn connect_now(shared: &Arc<Shared>) -> Result<Arc<Channel>, ConnectError> {
    let config = &shared.config;
    let allowed = shared.allow_install.load(Ordering::SeqCst);
    let launcher = shared.inner().settled.or(config.launcher);
    let options = Options { auth: config.auth.clone(), root: config.root.clone(), state: config.state.clone(), depot: config.depot.clone(), exit_idle: config.exit_idle, allow_install: allowed, helper: &*config.helper, launcher };
    let cancel = Arc::new(Cancel::default());
    *shared.cancel.lock().unwrap() = cancel.clone();
    if shared.leaving.load(Ordering::SeqCst) {
        cancel.cancel();
    }
    shared.with(|i| i.hello = None);
    let (channel, hello) = connect(&config.server, &config.transport, &options, &cancel, &|event| shared.on_event(event))?;
    shared.with(|i| {
        let hello_info = i.hello.get_or_insert_with(HelloInfo::default);
        (hello_info.node, hello_info.home, hello_info.slurm, hello_info.uploads) = (hello.node, hello.home.display().to_string(), hello.slurm, hello.uploads);
        hello_info.launcher = hello.launcher.map(|l| l.word().to_owned());
        i.settled = i.settled.or(hello.launcher);
    });
    Ok(Arc::new(channel))
}

/// Wait up to `time` for a start request. False when the session is closing.
fn wait_for(inbox: &Receiver<Msg>, time: Duration) -> bool {
    let until = Instant::now().checked_add(time);
    loop {
        let left = until.map_or(Duration::from_secs(3600), |until| until.saturating_duration_since(Instant::now()));
        match inbox.recv_timeout(left) {
            Ok(Msg::Kick) => return true,
            Ok(Msg::Quit) => return false,
            // Ends of connections that are over.
            Ok(Msg::Closed(_) | Msg::Started(..) | Msg::Notice(..)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) if until.is_some() => return true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// The runtime went away, for `why`. A start it cut short failed, and so did a takeover by another connection (`kept`:
/// attaching again would take it back and put the two at odds); a runtime that was up and ended is just not running,
/// and asking for one again starts one.
fn gone(i: &mut Inner, why: String, kept: bool) {
    let starting = i.starting();
    i.run = if starting { Run::Gone } else { Run::Idle };
    (i.wanted, i.job) = (None, None);
    i.step = Some(why.clone());
    i.state = if starting || kept { State::Failed(why) } else { State::NothingRunning };
}

/// Follow connection number `conn` until it ends. False when the session is closing.
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
                    // It went before its runtime was heard of, and was handled as gone: it is not ready.
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
                        i.state = State::Ready(RuntimeInfo {
                            port: runtime.port,
                            token: runtime.token,
                            mcp_url: runtime.mcp_url,
                            page_url: runtime.page_url,
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
                    // The connection ended under the start: `Closed` follows and takes the start along to the next one.
                    Err(StartError::Failed(message)) if message == CLOSED => {}
                    Err(_) if shared.with(|i| i.channel.as_ref().is_some_and(|c| c.is_closed())) => {}
                    Err(StartError::NeedsInstall(items)) => {
                        // The agreement came after this start began: it goes on with it.
                        if !installed && shared.with(|i| i.wanted.as_ref().is_some_and(|w| w.install) && std::mem::replace(&mut i.run, Run::Idle) == Run::Starting) {
                            shared.begin_start();
                            continue;
                        }
                        // What the listener tells callers names what is missing, not a restart that would only ask again.
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
                        // The wish may have become a start while the attach waited.
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
                        gone(i, text.clone(), false);
                        text
                    });
                    shared.trouble(text);
                    shared.listener.disconnected();
                }
                Notice::Replaced => {
                    let text = shared.with(|i| {
                        let text = format!("Another connection took Julia on {} over.", i.name);
                        gone(i, text.clone(), true);
                        text
                    });
                    shared.trouble(text);
                    shared.listener.disconnected();
                }
                // The helper is gone; `Closed` follows.
                Notice::Lost(message) => {
                    let name = shared.inner().name.clone();
                    shared.trouble(format!("{name}: the helper said: {message}"));
                }
            },
            _ => {}
        }
    }
}
