use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use super::Options;
use super::machines::LOCAL;
use crate::client::{Outcome, RuntimeInfo, Server, Session, State, Status, Want};
use crate::runtime::{self, Ended, Looked};
use crate::stopped;

pub(super) trait Provider: Send + Sync {
    fn ensure(&self, want: Want, wait: Duration, retry: bool) -> Outcome;
    fn status(&self) -> Status;
    fn stop(&self, force: bool) -> Result<(), String>;
    fn cluster(&self) -> bool;
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

#[derive(Clone)]
pub(super) struct Target {
    pub id: String,
    pub name: String,
    pub folder: Option<String>,
    pub active: bool,
    pub told: Option<u32>,
    pub failed: bool,
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

    pub(super) fn host(&self) -> Option<String> {
        (!self.is_local()).then(|| crate::mcp::clean_label(&self.name).unwrap_or_else(|| self.id.clone()))
    }
}

pub(super) fn local_server() -> Server {
    Server { id: LOCAL.into(), name: LOCAL.into(), ..Default::default() }
}

pub(super) struct Local {
    options: Options,
    state: Arc<(Mutex<Phase>, Condvar)>,
    builds: Mutex<Builds>,
    network_checked: Mutex<Option<u32>>,
    pinned: Mutex<Option<PathBuf>>,
}

#[derive(Default)]
struct Builds {
    checked: Option<u32>,
    stopped_one: bool,
    kept: Option<u32>,
}

pub(super) enum OtherBuild {
    Fine,
    Stopped,
    Kept(String),
}

const ANOTHER_START_POLL: Duration = Duration::from_millis(500);

enum Phase {
    Idle,
    Starting(String),
    Ready(Box<RuntimeInfo>),
    Failed(String),
}

impl Local {
    pub(super) fn new(options: Options) -> Local {
        Local { options, state: Arc::new((Mutex::new(Phase::Idle), Condvar::new())), builds: Mutex::default(), network_checked: Mutex::default(), pinned: Mutex::default() }
    }

    pub(super) fn state_dir(&self) -> PathBuf {
        let pinned = self.pinned.lock().unwrap().clone();
        pinned.unwrap_or_else(|| self.options.state_dir())
    }

    fn idle(&self, phase: &mut Phase) {
        *phase = Phase::Idle;
        *self.pinned.lock().unwrap() = None;
    }

    pub(super) fn other_build(&self, runtime: &RuntimeInfo, may_start: bool) -> OtherBuild {
        let mut builds = self.builds.lock().unwrap();
        if !runtime.reattached || builds.checked == Some(runtime.pid) {
            return OtherBuild::Fine;
        }
        let dir = &self.state_dir();
        let Some(state) = crate::read_state(dir).filter(|state| state.pid as u32 == runtime.pid) else { return OtherBuild::Fine };
        let this = crate::embedded::BUILD_VERSION;
        let removed = state.usable_as_is() && crate::julia_removed(&state, runtime.port);
        if state.usable_as_is() && !removed {
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
            match removed {
                true => eprintln!("endeavor: the runtime here (pid {}) runs on a Julia that was removed since it started, and no notebook is open in it; stopping it so that it starts again on the Julia here now", runtime.pid),
                false => eprintln!("endeavor: the runtime here (pid {}) was started by {which}, and no notebook is open in it; stopping it so that this build ({this}) starts its own", runtime.pid),
            }
            match self.stop(false) {
                Ok(()) => return OtherBuild::Stopped,
                Err(e) => eprintln!("endeavor: {e}"),
            }
        }
        if removed {
            return OtherBuild::Fine;
        }
        builds.kept = Some(runtime.pid);
        OtherBuild::Kept(other_build_notice(LOCAL, LOCAL, state.interface, open))
    }

    pub(super) fn network_differs(&self, runtime: &RuntimeInfo) -> Option<String> {
        let mut checked = self.network_checked.lock().unwrap();
        if !runtime.reattached || *checked == Some(runtime.pid) {
            return None;
        }
        let state = crate::read_state(&self.state_dir()).filter(|state| state.pid as u32 == runtime.pid)?;
        *checked = Some(runtime.pid);
        let recorded = state.network?;
        let ours = crate::network_settings();
        let differ: Vec<&str> = crate::NETWORK_VARS.iter().copied().filter(|name| recorded.get(*name) != ours.get(*name)).collect();
        (!differ.is_empty()).then(|| network_notice(&differ))
    }

    pub(super) fn kept(&self, pid: u32) -> bool {
        self.builds.lock().unwrap().kept == Some(pid)
    }

    fn phase(&self) -> MutexGuard<'_, Phase> {
        let ready = match &*self.state.0.lock().unwrap() {
            Phase::Ready(runtime) => Some(runtime.pid),
            _ => None,
        };
        let gone = ready.is_some_and(|pid| !matches!(runtime::look(&self.state_dir(), false, false), Looked::Running(state, _) if state.pid as u32 == pid));
        let mut phase = self.state.0.lock().unwrap();
        if gone && matches!(&*phase, Phase::Ready(runtime) if Some(runtime.pid) == ready) {
            self.idle(&mut phase);
        }
        phase
    }

    fn look(&self, dir: &Path, wait: Duration) -> Outcome {
        let looked_at = Instant::now();
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
            Looked::NotRunning | Looked::Dead(_) | Looked::Silent(_) if starting => Outcome::StillWorking("Another process is starting Endeavor".into()),
            Looked::NotRunning | Looked::Dead(_) => Outcome::NothingRunning,
            Looked::Running(state, port) => Outcome::Ready(announce(dir, &state, port, false)),
            Looked::OtherNode(state) => Outcome::Failed(runtime::other_node_text(&state.node)),
            Looked::Older(_) => Outcome::Failed(super::OLDER_RUNTIME_HERE.into()),
            Looked::Silent(state) if looked_at.elapsed() >= runtime::silent_wait() => Outcome::Failed(runtime::silent_text(state.pid)),
            Looked::Silent(state) => Outcome::Failed(format!("Endeavor on this computer (pid {}) is running but isn't answering. Try again in a moment.", state.pid)),
        }
    }

    fn begin(&self) {
        let dir = self.state_dir();
        *self.pinned.lock().unwrap() = Some(dir.clone());
        let options = Options { state_dir: dir, older_state_dir: None, ..self.options.clone() };
        let state = self.state.clone();
        std::thread::spawn(move || {
            let progress = |line: &str| {
                eprintln!("{line}");
                if let Phase::Starting(last) = &mut *state.0.lock().unwrap() {
                    *last = line.to_owned();
                }
            };
            let next = match super::start_or_reuse(&options, true, &progress, &|| false) {
                Ok(up) => Phase::Ready(Box::new(announce(&options.state_dir, &up.state, up.port, up.started))),
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

pub(super) fn other_build_notice(name: &str, id: &str, interface: Option<u32>, open: Option<u32>) -> String {
    let place = if name == LOCAL { "this computer" } else { name };
    let open = open.filter(|n| *n > 0).map_or(String::new(), |n| format!(" It has {n} notebook{} open.", if n == 1 { "" } else { "s" }));
    format!("Note: {}", crate::other_version_text(place, id, interface, &open))
}

pub(super) fn network_notice(differ: &[&str]) -> String {
    format!(
        "Note: Endeavor on this computer was started by another session, with other network settings than this session's ({}). Its downloads, package installs among them, use that session's settings. If one fails to reach the network, don't stop Endeavor yourself: tell the user, and ask whether to restart it, which closes the notebooks open in it. Only if the user says yes, call `stop_machine` with machine \"{LOCAL}\"; the next notebook call then starts it with this session's settings.",
        differ.join(", ")
    )
}

fn announce(dir: &Path, state: &crate::State, port: u16, started: bool) -> RuntimeInfo {
    eprintln!("Endeavor's notebooks: http://localhost:{port}/ (`endeavor open` lets a browser in)");
    if let Some(message) = (!started).then(|| super::other_build(dir)).flatten() {
        eprintln!("endeavor: {message}");
    }
    let page_token = state.page_token.as_deref().unwrap_or(&state.token);
    RuntimeInfo {
        port,
        mcp_url: format!("http://127.0.0.1:{port}/mcp"),
        page_url: format!("http://127.0.0.1:{port}/?token={}", page_token),
        page_token: page_token.to_owned(),
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
            self.idle(&mut phase);
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
                Phase::Idle if matches!(want, Want::Attach { .. }) => {
                    drop(phase);
                    let dir = self.state_dir();
                    let outcome = self.look(&dir, until.saturating_duration_since(Instant::now()));
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
                            *self.pinned.lock().unwrap() = Some(dir);
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
            Phase::Idle if runtime::lock_state(&self.state_dir()).is_held() => (State::Starting { queue: None }, None),
            Phase::Idle => (State::Connected, None),
            Phase::Starting(step) => (State::Starting { queue: None }, Some(step.clone()).filter(|step| !step.is_empty())),
            Phase::Ready(runtime) => (State::Ready((**runtime).clone()), None),
            Phase::Failed(why) => (State::Failed(why.clone()), None),
        };
        Status { machine: LOCAL.into(), name: LOCAL.into(), state, step, hello: None, job: None, gone: None }
    }

    fn stop(&self, force: bool) -> Result<(), String> {
        let dir = &self.state_dir();
        let starting = super::stop_lock(dir)?;
        let (events, _) = mpsc::channel();
        let began_here = matches!(*self.state.0.lock().unwrap(), Phase::Starting(_));
        let ended = runtime::end(dir, false, stopped::How::Connection, force, &events);
        drop(starting);
        let cancelled = matches!(ended, Ended::Cancelled(_));
        match ended {
            Ended::Stopped(_) | Ended::NotRunning | Ended::Cancelled(_) => {}
            Ended::Alive(pid) => return Err(format!("Endeavor (pid {pid}) is still running after the stop.")),
            Ended::Elsewhere(node) => return Err(format!("The Endeavor recorded here runs on {node}, not on this computer.")),
            Ended::Starting => return Err(runtime::STILL_STARTING_FORCE.into()),
            Ended::Unidentified => return Err(runtime::START_UNIDENTIFIED.into()),
        }
        let mut phase = self.state.0.lock().unwrap();
        if cancelled && began_here {
            let until = Instant::now() + Duration::from_secs(10);
            while matches!(*phase, Phase::Starting(_)) && Instant::now() < until {
                phase = self.state.1.wait_timeout(phase, until.saturating_duration_since(Instant::now())).unwrap().0;
            }
            if matches!(*phase, Phase::Failed(_)) {
                self.idle(&mut phase);
            }
        }
        if matches!(*phase, Phase::Ready(_)) {
            self.idle(&mut phase);
        }
        Ok(())
    }

    fn cluster(&self) -> bool {
        false
    }
}
