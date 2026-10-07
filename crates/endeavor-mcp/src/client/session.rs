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
use super::machines::Server;
use super::ssh::{Auth, Cancel, ConnectError, Event, NeedsInstall, Options, Transport, connect, start};

/// How long a reconnect waits after its first failure, doubling up to `RETRY_LAST`.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LAST: Duration = Duration::from_secs(30);

/// A connection that stays lost this long is given up on (state `failed`).
const RETRY_GIVE_UP: Duration = Duration::from_secs(10 * 60);

/// How often `reattach` asks the helper whether the runtime is there, and how long it waits between.
const REATTACH_TRIES: u32 = 3;
const REATTACH_PAUSE: Duration = Duration::from_secs(1);

/// Where a session stands.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Signing in and starting the helper, or getting the connection back.
    Connecting,
    /// The helper is up and no runtime is asked for (or it was stopped).
    Connected,
    /// A runtime is starting or being attached to.
    Starting,
    /// A cluster job waits in the queue.
    Queued,
    /// The runtime is attached and answers through the listener's port.
    Ready,
    /// The last step didn't work: `error` says why. Asking again tries again.
    Failed,
    /// Endeavor must install something on the machine first (`Status::needs_install`)
    /// and the user hasn't agreed. Not a failure, and not tried again by itself:
    /// asking again with `install`, or `Session::allow_install` for the helper, goes on.
    #[serde(rename = "needs_install")]
    NeedsInstall,
    /// A state this build doesn't know, from a link of another control protocol:
    /// not ready, and not replaceable. A session never has it.
    #[serde(other)]
    Unknown,
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
    /// In plain words, when `state` is `Failed`.
    pub error: Option<String>,
    /// What the helper reported, once it is connected.
    pub hello: Option<HelloInfo>,
    /// The runtime, once it is attached.
    pub runtime: Option<RuntimeInfo>,
    /// The cluster job, from the time it is submitted.
    pub job: Option<JobInfo>,
    /// Slurm's state and reason while the job waits.
    pub queue: Option<QueueInfo>,
    /// An attach (`Want::Attach`) found no runtime and started nothing.
    pub nothing_running: bool,
    /// With state `NeedsInstall`: what, and what was found on the machine.
    pub needs_install: Option<InstallInfo>,
}

/// What a caller wants of the machine's runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    /// Attach to a runtime that runs (on a cluster, a job that waits or runs) and start nothing
    /// otherwise. `install` is the user's agreement to the helper on the machine, if it lacks it.
    Attach { install: bool },
    /// Start the runtime, or attach to the one running. `job` is what to submit on a cluster.
    /// `install` is the user's agreement to what is missing on the machine: the helper, and for
    /// this start only, whatever the helper finds the start needs (such as Julia).
    Start { job: Option<JobRequest>, install: bool },
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
}

impl Config {
    /// Over `ssh` to `server`, with the machine's default folders. `helper` finds the helper to send to a machine that lacks it.
    pub fn new(server: Server, helper: impl Fn(&str, &str) -> Result<PathBuf, String> + Send + Sync + 'static) -> Config {
        let transport = Transport::for_server(&server);
        Config { server, transport, root: String::new(), state: String::new(), depot: String::new(), allow_install: false, messages: Messages::default(), helper: Box::new(helper) }
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
    /// A runtime start on this connection ended (`Inner::epoch` when it began).
    Started(u64, u64, Result<Runtime, StartError>),
    /// The attached runtime went away.
    Notice(u64, Notice),
}

/// What a start asked for.
#[derive(Clone, Debug, Default)]
struct Wish {
    /// On a cluster, what to submit.
    job: Option<JobRequest>,
    /// The start only attaches, and first asks the helper whether a runtime is there.
    check: bool,
    /// The helper may install what this start needs: the `install` of the
    /// request, and not what was agreed for an earlier one. Never for a start
    /// that only attaches.
    install: bool,
}

#[derive(PartialEq)]
enum Run {
    /// No runtime is attached or being started.
    Idle,
    /// A start is under way.
    Starting,
    /// The runtime went away before the end of its start was handled: that end is not a success.
    Gone,
    Attached(RuntimeInfo),
}

#[derive(Clone, Copy, PartialEq)]
enum Supervisor {
    /// Waiting for a `Kick`, or for the end of a pause between attempts.
    Waiting,
    /// A `Kick` is in its inbox.
    Kicked,
    /// Connecting, which a start request needn't wake it for.
    Connecting,
}

impl Inner {
    fn runtime(&self) -> Option<&RuntimeInfo> {
        match &self.run {
            Run::Attached(runtime) => Some(runtime),
            _ => None,
        }
    }

    fn starting(&self) -> bool {
        matches!(self.run, Run::Starting | Run::Gone)
    }
}

struct Inner {
    id: String,
    name: String,
    state: State,
    step: Option<String>,
    error: Option<String>,
    hello: Option<HelloInfo>,
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
    /// What the connection holds of the runtime.
    run: Run,
    /// Counts the stops, so the end of a start that one cut short is ignored.
    /// A stop counts before it asks the helper, which takes a while.
    epoch: u64,
    /// What the supervisor is doing.
    supervisor: Supervisor,
    /// The last such start found nothing (`Status::nothing_running`).
    nothing_running: bool,
    /// What the machine needs installed, while the state is `needs_install`.
    needs: Option<InstallInfo>,
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
    /// What `ensure` last answered `StillWorking` to, so that the next call with the same want
    /// tells how it ended instead of asking again.
    owed: Mutex<Option<Want>>,
}

/// One machine's connection. Closing it, or dropping it, detaches from the machine's helper
/// and ends its thread; the runtime goes on.
pub struct Session {
    shared: Arc<Shared>,
    supervisor: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// The name the agent knows a machine by.
fn display_name(server: &Server) -> String {
    [&server.name, &server.ssh_host, &server.id].into_iter().find(|n| !n.trim().is_empty()).cloned().unwrap_or_default()
}

impl Session {
    /// Open the listener and begin connecting to the machine in a thread of its own; nothing is
    /// asked of the runtime until `ensure`.
    pub fn new(config: Config) -> Result<Session, String> {
        let name = display_name(&config.server);
        let listener = Listener::new(&name, None, config.messages)?;
        let (inbox, messages) = mpsc::channel();
        let inner = Inner {
            id: config.server.id.clone(),
            name,
            state: State::Connecting,
            step: None,
            error: None,
            hello: None,
            job: None,
            queue: None,
            channel: None,
            conn: 0,
            wanted: None,
            resume: None,
            ended: None,
            run: Run::Idle,
            epoch: 0,
            supervisor: Supervisor::Waiting,
            nothing_running: false,
            needs: None,
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
            owed: Mutex::new(None),
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
    /// (`Outcome::StillWorking`). The work goes on meanwhile. The same want again while it is
    /// under way only waits for it; once it has ended with something other than `Ready`, the call
    /// that follows a `StillWorking` tells how, and the one after that tries again.
    pub fn ensure(&self, want: Want, wait: Duration) -> Outcome {
        let owed = self.shared.owed.lock().unwrap().as_ref() == Some(&want);
        let settled = owed.then(|| outcome(&self.shared.inner())).flatten();
        let outcome = settled.unwrap_or_else(|| {
            // A want that is under way is waited for, not asked again: that would cut a pause between attempts short.
            if !owed {
                self.request(&want);
            }
            self.wait(wait)
        });
        *self.shared.owed.lock().unwrap() = matches!(outcome, Outcome::StillWorking(_)).then_some(want);
        outcome
    }

    /// Ask for `want` and return at once; `status` has the rest. A start under way, or a runtime
    /// attached, is not an error. A start after a failure tries again.
    pub fn request(&self, want: &Want) {
        let (job, only_running, install) = match want {
            Want::Attach { install } => (None, true, *install),
            Want::Start { job, install } => (job.clone(), false, *install),
        };
        self.shared.request_start(job, only_running, install);
    }

    /// Where the session stands.
    pub fn status(&self) -> Status {
        let i = self.shared.inner();
        Status {
            machine: i.id.clone(),
            name: i.name.clone(),
            state: i.state,
            step: i.step.clone(),
            error: i.error.clone(),
            hello: i.hello.clone(),
            runtime: i.runtime().cloned(),
            job: i.job.clone(),
            queue: i.queue.clone(),
            nothing_running: i.nothing_running,
            needs_install: i.needs.clone(),
        }
    }

    /// Stop the runtime, for every client of it, and wait until it is gone (up to a minute and a
    /// bit). The connection stays, and the error says why when the runtime didn't stop.
    pub fn stop(&self) -> Result<(), String> {
        *self.shared.owed.lock().unwrap() = None;
        self.shared.request_stop()
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

    /// Detach from the machine's helper, close the listener and end the session's thread. The
    /// runtime keeps running. Calls after this answer `Outcome::Failed`.
    pub fn close(&self) {
        let shared = &self.shared;
        if shared.leaving.swap(true, Ordering::SeqCst) {
            return;
        }
        shared.cancel.lock().unwrap().cancel();
        let _ = shared.inbox.send(Msg::Quit);
        if let Some(supervisor) = self.supervisor.lock().unwrap().take() {
            let _ = supervisor.join();
        }
        if let Some(channel) = shared.with(|i| i.channel.take()) {
            // The helper goes when it has the word; a connection that is dead doesn't hold the close up.
            let (done, waited) = mpsc::channel();
            std::thread::spawn(move || {
                channel.detach();
                let _ = done.send(());
            });
            let _ = waited.recv_timeout(Duration::from_secs(5));
        }
        shared.listener.close();
        shared.with(|i| {
            (i.state, i.run, i.wanted) = (State::Failed, Run::Idle, None);
            i.error = Some(format!("Endeavor's connection to {} was closed.", i.name));
        });
    }

    fn wait(&self, wait: Duration) -> Outcome {
        let until = Instant::now() + wait;
        let mut inner = self.shared.inner.lock().unwrap();
        loop {
            if let Some(settled) = outcome(&inner) {
                return settled;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Outcome::StillWorking(inner.step.clone().unwrap_or_default());
            }
            inner = self.shared.changed.wait_timeout(inner, left).unwrap().0;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// How it stands, if that is settled.
fn outcome(i: &Inner) -> Option<Outcome> {
    match i.state {
        State::Ready => i.runtime().cloned().map(Outcome::Ready),
        State::Queued => i.queue.clone().map(|queue| Outcome::Queued { job: i.job.clone(), queue }),
        State::Failed => Some(Outcome::Failed(i.error.clone().unwrap_or_default())),
        State::NeedsInstall => i.needs.clone().map(Outcome::NeedsInstall),
        State::Connected if i.nothing_running => Some(Outcome::NothingRunning),
        _ => None,
    }
}

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let result = f(&mut self.inner.lock().unwrap());
        self.changed.notify_all();
        result
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
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
                if i.starting() {
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
            if i.run != Run::Idle {
                return None;
            }
            i.run = Run::Starting;
            i.wanted = Some(Wish { check: false, ..wish.clone() });
            i.ended = None;
            i.state = State::Starting;
            i.error = None;
            i.needs = None;
            i.queue = None;
            i.step = Some(format!("Starting the runtime on {}", i.name));
            Some((channel, wish, i.conn, i.epoch))
        });
        let Some((channel, wish, conn, epoch)) = begun else { return };
        let shared = self.clone();
        std::thread::spawn(move || {
            if wish.check && !shared.runtime_is_there(&channel, conn, epoch) {
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
            // A stop that came meanwhile has ended this start: nothing is attached to what it stopped.
            Ok(Reply::Runtime { runtime: RuntimeState::Running { .. } | RuntimeState::Queued { .. } }) => current(&self.inner()),
            Ok(Reply::Runtime { runtime: RuntimeState::NotRunning }) => {
                self.with(|i| {
                    if current(i) {
                        i.run = Run::Idle;
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
                        i.run = Run::Idle;
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

    /// The helper may be installed. A machine that lacks it has no connection, and is connected again.
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
        if i.supervisor == Supervisor::Waiting {
            i.supervisor = Supervisor::Kicked;
            let _ = self.inbox.send(Msg::Kick);
        }
    }

    fn request_start(self: &Arc<Shared>, job: Option<JobRequest>, only_running: bool, install: bool) {
        let connected = self.with(|i| {
            // Checked under the lock, as a close that came meanwhile has ended the supervisor and taken the connection.
            if self.leaving.load(Ordering::SeqCst) {
                return false;
            }
            // Only a start with no connection can mean the helper: with one, `install` is for what the start needs, and a
            // yes to that mustn't be kept as one for the helper. Set under the lock, as the supervisor checks it.
            if install && i.channel.is_none() {
                self.allow_install.store(true, Ordering::SeqCst);
            }
            if i.run != Run::Idle {
                return true;
            }
            i.wanted = Some(Wish { job, check: only_running, install: install && !only_running });
            i.resume = None;
            i.nothing_running = false;
            if i.channel.is_some() {
                return true;
            }
            // No connection: the supervisor starts the runtime once it has one, and
            // tries now if it was waiting. One wake-up is enough, and none while it connects.
            if matches!(i.state, State::Failed | State::NeedsInstall) {
                (i.state, i.error, i.needs) = (State::Connecting, None, None);
            }
            if i.supervisor == Supervisor::Waiting {
                i.supervisor = Supervisor::Kicked;
                let _ = self.inbox.send(Msg::Kick);
            }
            false
        });
        if connected {
            self.begin_start();
        }
    }

    fn request_stop(&self) -> Result<(), String> {
        // Counted before the helper is asked: it takes a while, and the end of a start it cuts short is no failure.
        let Some((channel, mine)) = self.with(|i| {
            let channel = i.channel.clone()?;
            i.epoch += 1;
            Some((channel, i.epoch))
        }) else {
            return Err(not_connected_to_stop(&self.inner()));
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
                        i.run = Run::Idle;
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
                    if same && i.starting() {
                        i.run = Run::Idle;
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
            i.supervisor = Supervisor::Connecting;
            i.needs = None;
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
                    eprintln!("The helper isn't installed on {name}; waiting for the user's agreement to install it");
                    let parked = shared.with(|i| {
                        // Checked with the lock held, so that a permission is never lost between the two.
                        if shared.allow_install.load(Ordering::SeqCst) {
                            return false;
                        }
                        i.supervisor = Supervisor::Waiting;
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
                    if !wait_for(inbox, Duration::MAX) {
                        return;
                    }
                    continue;
                }
                let lost = *lost_since.get_or_insert_with(Instant::now);
                let retry = reconnecting && error.retry && lost.elapsed() < RETRY_GIVE_UP;
                eprintln!("The connection to {name} failed{}: {}", if retry { ", trying again" } else { "" }, error.message);
                shared.with(|i| {
                    i.supervisor = Supervisor::Waiting;
                    i.state = if retry { State::Connecting } else { State::Failed };
                    i.error = (!retry).then(|| error.message.clone());
                    i.step = Some(if retry { format!("Lost the connection to {name}: {}", error.message) } else { error.message.clone() });
                });
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
                    i.supervisor = Supervisor::Waiting;
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
/// helper says it is still there (running, or a job that waits): a start would
/// otherwise begin a new runtime, or on a cluster submit a job nobody asked for.
/// An answer that isn't clear is asked for again, and then leaves the session `Failed`.
fn reattach(shared: &Shared, channel: &Channel, wish: Wish) {
    let mut trouble = String::new();
    for attempt in 0..REATTACH_TRIES {
        if shared.leaving.load(Ordering::SeqCst) {
            return;
        }
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

/// Connect to the machine with the record the session was made with.
fn connect_now(shared: &Arc<Shared>) -> Result<Arc<Channel>, ConnectError> {
    let config = &shared.config;
    let allowed = shared.allow_install.load(Ordering::SeqCst);
    let options = Options { auth: Auth::Batch, root: config.root.clone(), state: config.state.clone(), depot: config.depot.clone(), exit_idle: true, allow_install: allowed, helper: &*config.helper };
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
                    i.resume = (i.run != Run::Idle).then(|| i.wanted.take().or_else(|| i.resume.take()).unwrap_or_default());
                    i.ended = (i.resume.is_none() && i.state == State::Failed).then(|| i.error.take()).flatten();
                    i.channel = None;
                    i.run = Run::Idle;
                    i.queue = None;
                    i.state = State::Connecting;
                    i.error = None;
                    i.step = Some(format!("Lost the connection to {}; connecting again", i.name));
                });
                return true;
            }
            Msg::Started(g, epoch, result) if g == conn => {
                let current = shared.with(|i| i.epoch == epoch);
                if !current {
                    continue;
                }
                match result {
                    // It went before its runtime was heard of, and was handled as gone: it is not ready.
                    Ok(_) if shared.with(|i| i.run == Run::Gone) => shared.with(|i| i.run = Run::Idle),
                    Ok(runtime) => shared.with(|i| {
                        i.state = State::Ready;
                        i.error = None;
                        i.queue = None;
                        i.step = Some(format!("Ready on {}", runtime.node));
                        if let Some(job) = &runtime.job {
                            let known = i.job.take().unwrap_or_default();
                            i.job = Some(JobInfo { id: job.id.clone(), summary: known.summary, node: Some(job.node.clone()), ends_at: job.ends_at });
                        }
                        i.run = Run::Attached(RuntimeInfo {
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
                            (i.run, i.job, i.queue) = (Run::Idle, None, None);
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
                            i.run = Run::Idle;
                            i.wanted = None;
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
                        i.run = if i.starting() { Run::Gone } else { Run::Idle };
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
                        i.run = if i.starting() { Run::Gone } else { Run::Idle };
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
