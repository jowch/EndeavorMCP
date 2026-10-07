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
    /// when nothing is known to run. A failure is kept and given to every call until one asks to `retry`.
    fn ensure(&self, want: Want, wait: Duration, retry: bool) -> Outcome;
    fn status(&self) -> Status;
    /// End the runtime for every client of it, and wait until it is gone.
    fn stop(&self) -> Result<(), String>;
    /// Whether the machine is a Slurm cluster, by the record the connection was made with.
    fn cluster(&self) -> bool;
}

impl Provider for Session {
    fn ensure(&self, want: Want, wait: Duration, retry: bool) -> Outcome {
        Session::ensure(self, want, wait, retry)
    }

    fn status(&self) -> Status {
        Session::status(self)
    }

    fn stop(&self) -> Result<(), String> {
        Session::stop(self).map_err(|why| {
            // Nothing to do about a stop the helper refused.
            if self.connected() {
                return why;
            }
            match self.status().state {
                State::NeedsInstall => format!("{why} Ask the user whether Endeavor may install it, then call `stop_machine` again with `install: true`."),
                State::Failed => format!("{why} Tell the user, and call `stop_machine` again once that is fixed."),
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
    /// The session's folder there, if `use_machine` was given one; else the server's home.
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

    pub(super) fn local(folder: &std::path::Path) -> Target {
        Target::new(&local_server(), Some(folder.display().to_string()))
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
}

enum Phase {
    /// The state folder says what runs.
    Idle,
    /// A start is under way, at this step.
    Starting(String),
    Ready(RuntimeInfo),
    /// A start ended with this, which every call is told until one asks to try again.
    Failed(String),
}

impl Local {
    pub(super) fn new(options: Options) -> Local {
        Local { options, state: Arc::new((Mutex::new(Phase::Idle), Condvar::new())) }
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

    /// The runtime recorded in the state folder, if there is one; none is started.
    fn look(&self) -> Outcome {
        match runtime::look(&self.options.state_dir, false, true) {
            Looked::NotRunning | Looked::Dead(_) => Outcome::NothingRunning,
            Looked::Running(state, port) => Outcome::Ready(announce(&self.options, &state, port, false)),
            Looked::OtherNode(state) => Outcome::Failed(runtime::other_node_text(&state.node)),
            Looked::Older(_) => Outcome::Failed(super::OLDER_RUNTIME_HERE.into()),
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
                Ok(up) => Phase::Ready(announce(&options, &up.state, up.port, up.started)),
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

/// A runtime that was found or started, as the session uses it. The user is told on stderr where the
/// notebooks are, and when another build started it.
fn announce(options: &Options, state: &crate::State, port: u16, started: bool) -> RuntimeInfo {
    eprintln!("Endeavor's notebooks: http://localhost:{port}/?token={}", state.token);
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
                Phase::Ready(runtime) => return Outcome::Ready(runtime.clone()),
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
                    let outcome = self.look();
                    if let Outcome::Ready(runtime) = &outcome {
                        let mut phase = self.state.0.lock().unwrap();
                        if matches!(*phase, Phase::Idle) {
                            *phase = Phase::Ready(runtime.clone());
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
        let (state, step, error, runtime) = match &*self.phase() {
            // A start another process has under way is a start under way.
            Phase::Idle if runtime::starting(&self.options.state_dir) => (State::Starting, None, None, None),
            Phase::Idle => (State::Connected, None, None, None),
            Phase::Starting(step) => (State::Starting, Some(step.clone()).filter(|step| !step.is_empty()), None, None),
            Phase::Ready(runtime) => (State::Ready, None, None, Some(runtime.clone())),
            Phase::Failed(why) => (State::Failed, None, Some(why.clone()), None),
        };
        Status { machine: LOCAL.into(), name: LOCAL.into(), state, step, error, hello: None, runtime, job: None, queue: None, nothing_running: false, needs_install: None }
    }

    fn stop(&self) -> Result<(), String> {
        let dir = &self.options.state_dir;
        // First, so that a start another process has under way is waited for, not missed.
        let _starting = super::stop_lock(dir)?;
        let (events, _) = mpsc::channel();
        match runtime::end(dir, false, stopped::How::Connection, &events) {
            Ended::Stopped(_) | Ended::NotRunning => {}
            Ended::Alive(pid) => return Err(format!("Julia (pid {pid}) is still running after the stop.")),
            Ended::Elsewhere(node) => return Err(format!("The Julia recorded here runs on {node}, not on this computer.")),
            Ended::Starting => return Err(runtime::STILL_STARTING.into()),
        }
        let mut phase = self.state.0.lock().unwrap();
        if matches!(*phase, Phase::Ready(_)) {
            *phase = Phase::Idle;
        }
        Ok(())
    }

    fn cluster(&self) -> bool {
        false
    }
}
