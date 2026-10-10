//! Where a session's notebooks run (docs/plugins-and-remote.md): on this computer or on a machine. The
//! two differ in how the runtime's address is obtained and how it is stopped, which is a `Provider`:
//! `Local` finds, starts and ends the runtime in the state folder directly, and a machine's is its
//! `client::Session`. Both answer with the same `Outcome` and `Status`.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use super::Options;
use super::machines::LOCAL;
use crate::client::{Outcome, RuntimeInfo, Server, Session, State, Status, Want};
use crate::runtime::{self, Ended, Looked};
use crate::stopped;

/// Finding, starting, looking at and ending one runtime.
pub(super) trait Provider: Send + Sync {
    /// Ask for `want` and say how it stands, as soon as that is known or `wait` has passed
    /// (`Outcome::StillWorking`); a start goes on meanwhile. An `Attach` starts nothing and looks afresh
    /// when nothing is known to run, waiting for a start that another process has under way. A failure is kept and given to every call until one asks to `retry`.
    fn ensure(&self, want: Want, wait: Duration, retry: bool) -> Outcome;
    fn status(&self) -> Status;
    /// End the runtime for every client of it, and wait until it is gone. With `force` a start that is
    /// under way is cancelled, whoever began it; without, only one this session's connection is waiting on.
    fn stop(&self, force: bool) -> Result<(), String>;
    /// Whether the machine is a Slurm cluster, by the record the connection was made with.
    fn cluster(&self) -> bool;
    /// Ask a machine's helper about its files, once it is connected (`Session::files`). None on this
    /// computer, and on a machine that isn't connected by the end of `wait`.
    fn files(&self, _request: wire::files::Request, _wait: Duration) -> Option<Result<wire::files::Reply, String>> {
        None
    }
}

impl Provider for Session {
    fn ensure(&self, want: Want, wait: Duration, retry: bool) -> Outcome {
        Session::ensure(self, want, wait, retry)
    }

    fn files(&self, request: wire::files::Request, wait: Duration) -> Option<Result<wire::files::Reply, String>> {
        Session::files(self, request, wait)
    }

    fn status(&self) -> Status {
        Session::status(self)
    }

    fn stop(&self, force: bool) -> Result<(), String> {
        let stopped = if force { Session::force_stop(self) } else { Session::stop(self) };
        stopped.map_err(|why| {
            // Nothing to do about a stop the helper refused.
            if self.connected() {
                return why;
            }
            match self.status().state {
                State::NeedsInstall(_) => format!("{why} Ask the user whether Endeavor may install it, then call `stop_machine` again with `install: true`."),
                State::Failed(_) => format!("{why} Tell the user, and call `stop_machine` again once that is fixed."),
                _ => format!("{why} Wait a few seconds, then call `stop_machine` again."),
            }
        })
    }

    fn cluster(&self) -> bool {
        Session::cluster(self)
    }
}

/// Where this session's notebooks run: this computer (`id` is `LOCAL`) or a machine.
#[derive(Clone)]
pub(super) struct Target {
    /// The id in the machines file, which the connection goes by.
    pub id: String,
    /// What the agent calls it.
    pub name: String,
    /// The session's folder there, if `use_machine` was given one; else the server's home. On this
    /// computer none means the front was started with `--no-folder`.
    pub folder: Option<String>,
    /// The session should have a runtime there: false after `stop_machine`.
    pub active: bool,
    /// The runtime (its pid) that was told this session's folder.
    pub told: Option<u32>,
    /// The last notebook call that needed a runtime reported a failure, so the next one asks to try again.
    pub failed: bool,
    /// How many times `use_machine` has set the target: a call that waited sees that it did, even if the session is back where it was.
    pub moves: u64,
}

impl Target {
    pub(super) fn new(server: &Server, folder: Option<String>) -> Target {
        Target { id: server.id.clone(), name: server.display_name(), folder, active: true, told: None, failed: false, moves: 0 }
    }

    pub(super) fn local(folder: Option<&std::path::Path>) -> Target {
        Target::new(&local_server(), folder.map(|folder| folder.display().to_string()))
    }

    pub(super) fn is_local(&self) -> bool {
        self.id == LOCAL
    }

    /// The name a machine's runtime knows this session's computer by (`X-Endeavor-Host`); a runtime on this computer is sent none.
    pub(super) fn host(&self) -> Option<String> {
        (!self.is_local()).then(|| crate::mcp::clean_label(&self.name).unwrap_or_else(|| self.id.clone()))
    }
}

/// This computer, as the record `Target::new` and the tools take.
pub(super) fn local_server() -> Server {
    Server { id: LOCAL.into(), name: LOCAL.into(), ..Default::default() }
}

/// This computer's runtime, in the state folder `serve` and `mcp` share. A start runs in a thread of its own
/// that goes on when the call that asked for it stops waiting.
pub(super) struct Local {
    options: Options,
    state: Arc<(Mutex<Phase>, Condvar)>,
    builds: Mutex<Builds>,
}

/// What `Local::other_build` has done so far.
#[derive(Default)]
struct Builds {
    /// The runtime whose build was checked.
    checked: Option<u32>,
    /// A runtime of another build was stopped. It is done once, so that two fronts of different builds don't take turns.
    stopped_one: bool,
    /// The runtime of another build that was kept, and the agent told of.
    kept: Option<u32>,
}

/// What `Local::other_build` came to.
pub(super) enum OtherBuild {
    /// The runtime is this build's, was checked already, or is left for a call that may start one.
    Fine,
    /// It was another build's and idle, and was stopped: the call starts one of this build.
    Stopped,
    /// It is another build's and was kept: what to tell the agent, once.
    Kept(String),
}

/// How often a look waiting for another process's start looks again.
const ANOTHER_START_POLL: Duration = Duration::from_millis(500);

enum Phase {
    /// The state folder says what runs.
    Idle,
    /// A start is under way, at this step.
    Starting(String),
    Ready(Box<RuntimeInfo>),
    /// A start ended with this, which every call is told until one asks to try again.
    Failed(String),
}

impl Local {
    pub(super) fn new(options: Options) -> Local {
        Local { options, state: Arc::new((Mutex::new(Phase::Idle), Condvar::new())), builds: Mutex::default() }
    }

    /// This front lists its own build's tools, and the runtime it found running here (`runtime`) runs the
    /// calls. One of another build that offers this build's interface (`core::INTERFACE`) is used as it is.
    /// Otherwise, once per runtime: one another build started is stopped if a front started it in the
    /// background (it ends itself when idle, so nobody relies on its port), no notebook is open in it, and
    /// the call may start a runtime (`may_start`), so that the call starts one of this build. Any other is
    /// kept, and the agent is told. A runtime with no notebook open is left for a call that may start one.
    pub(super) fn other_build(&self, runtime: &RuntimeInfo, may_start: bool) -> OtherBuild {
        let mut builds = self.builds.lock().unwrap();
        if !runtime.reattached || builds.checked == Some(runtime.pid) {
            return OtherBuild::Fine;
        }
        let dir = &self.options.state_dir;
        let Some(state) = crate::read_state(dir).filter(|state| state.pid as u32 == runtime.pid) else { return OtherBuild::Fine };
        let this = crate::embedded::BUILD_VERSION;
        if state.usable_as_is() {
            builds.checked = Some(runtime.pid);
            return OtherBuild::Fine;
        }
        let open = crate::open_notebooks(runtime.port, &runtime.token);
        let idle = open == Some(0) && state.exits_when_idle == Some(true) && !builds.stopped_one;
        if idle && !may_start {
            return OtherBuild::Fine;
        }
        builds.checked = Some(runtime.pid);
        if idle {
            let which = crate::which_build(state.build.as_deref());
            builds.stopped_one = true;
            eprintln!("endeavor: Julia here (pid {}) was started by {which}, and no notebook is open in it; stopping it so that this build ({this}) starts its own", runtime.pid);
            match self.stop(false) {
                Ok(()) => return OtherBuild::Stopped,
                Err(e) => eprintln!("endeavor: {e}"),
            }
        }
        builds.kept = Some(runtime.pid);
        OtherBuild::Kept(other_build_notice(LOCAL, LOCAL, state.interface, open))
    }

    /// Whether `pid` is a runtime of another build that `other_build` kept. One it left for a call that may
    /// start a runtime isn't, since that call stops it.
    pub(super) fn kept(&self, pid: u32) -> bool {
        self.builds.lock().unwrap().kept == Some(pid)
    }

    /// The phase, with a runtime that is no longer the one recorded forgotten. The state folder is read with the lock let go.
    fn phase(&self) -> MutexGuard<'_, Phase> {
        let ready = match &*self.state.0.lock().unwrap() {
            Phase::Ready(runtime) => Some(runtime.pid),
            _ => None,
        };
        let gone = ready.is_some_and(|pid| !matches!(runtime::look(&self.options.state_dir, false, false), Looked::Running(state, _) if state.pid as u32 == pid));
        let mut phase = self.state.0.lock().unwrap();
        if gone && matches!(&*phase, Phase::Ready(runtime) if Some(runtime.pid) == ready) {
            *phase = Phase::Idle;
        }
        phase
    }

    /// The runtime recorded in the state folder, if there is one; none is started. One that is alive and
    /// doesn't answer is asked again for up to `wait`, and counts as stuck once `silent_wait` has passed.
    fn look(&self, wait: Duration) -> Outcome {
        let dir = &self.options.state_dir;
        let looked_at = Instant::now();
        // Before the look: a core writes its record before it lets go of the lock.
        let starting = runtime::lock_state(dir).is_held();
        let mut looked = runtime::look(dir, false, true);
        if matches!(looked, Looked::Silent(_)) && !starting && !wait.is_zero() {
            let pause = &mut || {
                std::thread::sleep(Duration::from_millis(200));
                true
            };
            looked = runtime::ask_again(dir, false, looked_at, wait.min(runtime::silent_wait()), pause).expect("a pause that never stops the wait");
        }
        match looked {
            // A start another process has under way is a start under way, not nothing.
            Looked::NotRunning | Looked::Dead(_) | Looked::Silent(_) if starting => Outcome::StillWorking("Another process is starting Julia".into()),
            Looked::NotRunning | Looked::Dead(_) => Outcome::NothingRunning,
            Looked::Running(state, port) => Outcome::Ready(announce(&self.options, &state, port, false)),
            Looked::OtherNode(state) => Outcome::Failed(runtime::other_node_text(&state.node)),
            Looked::Older(_) => Outcome::Failed(super::OLDER_RUNTIME_HERE.into()),
            Looked::Silent(state) if looked_at.elapsed() >= runtime::silent_wait() => Outcome::Failed(runtime::silent_text(state.pid)),
            Looked::Silent(state) => Outcome::Failed(format!("Julia on this computer (pid {}) is running but isn't answering. Try again in a moment.", state.pid)),
        }
    }

    fn begin(&self) {
        let (options, state) = (self.options.clone(), self.state.clone());
        std::thread::spawn(move || {
            let progress = |line: &str| {
                eprintln!("{line}");
                if let Phase::Starting(last) = &mut *state.0.lock().unwrap() {
                    *last = line.to_owned();
                }
            };
            let next = match super::start_or_reuse(&options, true, &progress, &|| false) {
                Ok(up) => Phase::Ready(Box::new(announce(&options, &up.state, up.port, up.started))),
                Err(e) => {
                    eprintln!("endeavor: {e}");
                    Phase::Failed(e)
                }
            };
            *state.0.lock().unwrap() = next;
            state.1.notify_all();
        });
    }
}

/// What the agent is told once about a runtime on the machine `name` (id `id`) that another build started
/// and that doesn't offer this build's interface, whose core offers `interface`; `open` is how many notebooks
/// are open in it, when that is known.
pub(super) fn other_build_notice(name: &str, id: &str, interface: Option<u32>, open: Option<u32>) -> String {
    let place = if name == LOCAL { "this computer" } else { name };
    let open = open.filter(|n| *n > 0).map_or(String::new(), |n| format!(" It has {n} notebook{} open.", if n == 1 { "" } else { "s" }));
    format!("Note: {}", crate::other_version_text(place, id, interface, &open))
}

/// A runtime that was found or started, as the session uses it. The user is told on stderr where the
/// notebooks are, and when another build started it. Not with the token: agents' clients keep stderr in
/// their logs.
fn announce(options: &Options, state: &crate::State, port: u16, started: bool) -> RuntimeInfo {
    eprintln!("Endeavor's notebooks: http://localhost:{port}/ (`endeavor open` lets a browser in)");
    if let Some(message) = (!started).then(|| super::other_build(&options.state_dir)).flatten() {
        eprintln!("endeavor: {message}");
    }
    RuntimeInfo {
        port,
        mcp_url: format!("http://127.0.0.1:{port}/mcp"),
        page_url: format!("http://127.0.0.1:{port}/?token={}", state.token),
        token: state.token.clone(),
        node: state.node.clone(),
        pid: state.pid as u32,
        reattached: !started,
        job: None,
        remote_port: None,
        build: state.build.clone(),
        interface: state.interface,
    }
}

impl Provider for Local {
    fn ensure(&self, want: Want, wait: Duration, retry: bool) -> Outcome {
        let until = Instant::now() + wait;
        let mut phase = self.phase();
        if retry && matches!(*phase, Phase::Failed(_)) {
            *phase = Phase::Idle;
        }
        loop {
            match &*phase {
                Phase::Ready(runtime) => return Outcome::Ready((**runtime).clone()),
                Phase::Failed(why) => return Outcome::Failed(why.clone()),
                Phase::Starting(step) => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Outcome::StillWorking(step.clone());
                    }
                    phase = self.state.1.wait_timeout(phase, left).unwrap().0;
                }
                // Looked at with the phase let go: a runtime that is slow to answer holds up no other call.
                Phase::Idle if matches!(want, Want::Attach { .. }) => {
                    drop(phase);
                    let outcome = self.look(until.saturating_duration_since(Instant::now()));
                    // A start another process has under way is waited for, like one of this process's.
                    let left = until.saturating_duration_since(Instant::now());
                    if matches!(outcome, Outcome::StillWorking(_)) && !left.is_zero() {
                        std::thread::sleep(left.min(ANOTHER_START_POLL));
                        phase = self.phase();
                        continue;
                    }
                    if let Outcome::Ready(runtime) = &outcome {
                        let mut phase = self.state.0.lock().unwrap();
                        if matches!(*phase, Phase::Idle) {
                            *phase = Phase::Ready(Box::new(runtime.clone()));
                        }
                    }
                    return outcome;
                }
                Phase::Idle => {
                    *phase = Phase::Starting(String::new());
                    self.begin();
                }
            }
        }
    }

    fn status(&self) -> Status {
        let (state, step) = match &*self.phase() {
            // A start another process has under way is a start under way.
            Phase::Idle if runtime::lock_state(&self.options.state_dir).is_held() => (State::Starting { queue: None }, None),
            Phase::Idle => (State::Connected, None),
            Phase::Starting(step) => (State::Starting { queue: None }, Some(step.clone()).filter(|step| !step.is_empty())),
            Phase::Ready(runtime) => (State::Ready((**runtime).clone()), None),
            Phase::Failed(why) => (State::Failed(why.clone()), None),
        };
        Status { machine: LOCAL.into(), name: LOCAL.into(), state, step, hello: None, job: None }
    }

    fn stop(&self, force: bool) -> Result<(), String> {
        let dir = &self.options.state_dir;
        // First, so that a start another process has under way is waited for, not missed.
        let starting = super::stop_lock(dir)?;
        let (events, _) = mpsc::channel();
        // Before the stop, which takes a while: a start of this process that ends by it is over by then.
        let began_here = matches!(*self.state.0.lock().unwrap(), Phase::Starting(_));
        let ended = runtime::end(dir, false, stopped::How::Connection, force, &events);
        // Let go before waiting for this process's own start to settle: it may wait for the lock.
        drop(starting);
        let cancelled = matches!(ended, Ended::Cancelled(_));
        match ended {
            Ended::Stopped(_) | Ended::NotRunning | Ended::Cancelled(_) => {}
            Ended::Alive(pid) => return Err(format!("Julia (pid {pid}) is still running after the stop.")),
            Ended::Elsewhere(node) => return Err(format!("The Julia recorded here runs on {node}, not on this computer.")),
            Ended::Starting => return Err(runtime::STILL_STARTING_FORCE.into()),
            Ended::Unidentified => return Err(runtime::START_UNIDENTIFIED.into()),
        }
        let mut phase = self.state.0.lock().unwrap();
        // A start of this process that was cancelled ends in a failure, which is its own doing and is not kept.
        if cancelled && began_here {
            let until = Instant::now() + Duration::from_secs(10);
            while matches!(*phase, Phase::Starting(_)) && Instant::now() < until {
                phase = self.state.1.wait_timeout(phase, until.saturating_duration_since(Instant::now())).unwrap().0;
            }
            if matches!(*phase, Phase::Failed(_)) {
                *phase = Phase::Idle;
            }
        }
        if matches!(*phase, Phase::Ready(_)) {
            *phase = Phase::Idle;
        }
        Ok(())
    }

    fn cluster(&self) -> bool {
        false
    }
}
