//! Finding the runtime in a state folder, or starting one, and stopping it: the one place that does
//! each, for the helper (`endeavor connect`), `serve` and `mcp`. The callers differ in what they say
//! while it goes on and how they end it (`Hooks`), and in the words for what comes of it.
//!
//! A start, once begun, finishes without the process that asked for it. The core is its own session. As
//! the first thing it does it holds `starting.lock` in the state folder, and it lets go only after it has
//! written `runtime.json`; the OS lets go if it dies. So a held `starting.lock` with no usable record
//! means a start is under way, and one that is free with no record means there is none. The process that
//! spawned the core holds `start.lock` until the core holds its own lock, and waits for the runtime
//! without it. Only that process can end the start (it has the child); a client that waits for another
//! process's start never stops it.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::{Args, Event, Runtime, State, julia, standalone, stopped};

/// What a state folder holds, found without starting or stopping anything.
pub(crate) enum Looked {
    /// No record.
    NotRunning,
    /// A record whose process is gone.
    Dead(State),
    /// Alive, and answering on its port when asked to (`look`'s `ping`).
    Running(State, u16),
    /// Recorded by another machine of the same home folder, and this isn't asked to take it.
    OtherNode(State),
    /// Alive, from a build before one port per runtime.
    Older(State),
    /// Alive, with a port, and not answering.
    Silent(State),
}

impl Looked {
    /// The state of a runtime whose process is alive, whether or not it answers.
    pub(crate) fn alive(self) -> Option<State> {
        match self {
            Looked::Running(state, _) | Looked::Older(state) | Looked::Silent(state) => Some(state),
            Looked::NotRunning | Looked::Dead(_) | Looked::OtherNode(_) => None,
        }
    }
}

/// The runtime in `dir`. With `any_node` the record of another node is taken as this machine's:
/// the state folder belongs to this one machine, which was renamed. Without `ping` a process that is
/// alive is `Running`, and the port is not asked.
pub(crate) fn look(dir: &Path, any_node: bool, ping: bool) -> Looked {
    let Some(state) = crate::read_state(dir) else { return Looked::NotRunning };
    if state.node != crate::hostname() && !any_node {
        return Looked::OtherNode(state);
    }
    if !crate::pid_alive(state.pid, state.started) {
        return Looked::Dead(state);
    }
    match state.port {
        None => Looked::Older(state),
        // Twice: a busy runtime can be slow to answer once.
        Some(port) if !ping || answers(port, &state.token) || answers(port, &state.token) => Looked::Running(state, port),
        Some(_) => Looked::Silent(state),
    }
}

/// Whether the runtime on `port` answers its calls.
fn answers(port: u16, token: &str) -> bool {
    crate::bridge_call(port, crate::CALL, token, "ping").is_ok_and(|status| status == 200)
}

/// Why the runtime recorded in a state folder is not used.
pub(crate) fn other_node_text(node: &str) -> String {
    let here = crate::hostname();
    format!("Julia for this folder is running on {node}, and this is {here}. Connect to {node} to use it, or stop it there.")
}

/// What the callers of `find_or_start` give it.
pub(crate) struct Want<'a> {
    pub args: &'a Args,
    /// The notebook system to start; only Pluto is built.
    pub engine: &'a str,
    /// Whether Julia may be downloaded when none is found.
    pub install: bool,
    /// The folder `runtime/` was unpacked to, asked for only when a runtime is started.
    pub runtime: &'a dyn Fn() -> Result<PathBuf, String>,
    /// Where a runtime reports its exit.
    pub events: &'a Sender<Event>,
}

/// What a caller says and decides while a start goes on.
pub(crate) trait Hooks {
    /// A line about how the start is going: Julia's download, the runtime's log.
    fn progress(&mut self, line: String);
    /// Julia was found, or installed.
    fn found(&mut self, version: &str, path: &str);
    /// Wait up to `wait`, and hear what the caller has to say meanwhile. False ends the wait. It stops
    /// a runtime only if this process started it.
    fn wait(&mut self, wait: Duration, waiting: Waiting) -> bool;
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Waiting {
    /// For the start lock, which is held for a look and a spawn, and for Julia to be found.
    Lock,
    /// For a runtime another process is starting.
    Other,
    /// For a runtime this process started.
    Ready,
}

/// A runtime that can be used.
pub(crate) struct Up {
    pub state: State,
    pub port: u16,
    pub runtime: Runtime,
    /// This call started it.
    pub started: bool,
}

pub(crate) enum Outcome {
    Ready(Up),
    /// `OtherNode` or `Older`: the record can't be used, and no runtime was started.
    Unusable(Looked),
    /// Julia is not there and `install` was false.
    NeedsInstall(Vec<wire::Item>),
    Failed(String),
    /// It exited while starting.
    Died { status: String, log_tail: Vec<String> },
    /// `Hooks::wait` said to stop.
    Cancelled,
}

const POLL: Duration = Duration::from_millis(200);

/// How long a core has to take its `starting.lock`, which is the first thing it does.
const CORE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Take the start lock, which is held for a look and a spawn.
pub(crate) fn take_start_lock(dir: &Path, hooks: &mut dyn Hooks) -> Result<File, Outcome> {
    let waited = standalone::wait_for_start_lock(dir, standalone::start_lock_limit(), || if hooks.wait(POLL, Waiting::Lock) { Ok(()) } else { Err(()) });
    waited.map_err(|wait| match wait {
        standalone::Wait::Failed(message) => Outcome::Failed(message),
        standalone::Wait::TimedOut => Outcome::Failed(standalone::start_lock_gave_up(dir)),
        standalone::Wait::Interrupted(()) => Outcome::Cancelled,
    })
}

/// Find the runtime in `want.args.state_dir`, or start one, and wait until it answers. Callers asked at
/// once start one runtime and the rest wait for it.
pub(crate) fn find_or_start(want: &Want, hooks: &mut dyn Hooks) -> Outcome {
    let dir = &want.args.state_dir;
    if let Err(e) = crate::make_state_dir(dir) {
        return Outcome::Failed(e);
    }
    loop {
        let lock = match take_start_lock(dir, hooks) {
            Ok(lock) => lock,
            Err(outcome) => return outcome,
        };
        // Before the look: a core that records itself and lets go in between is then found by the look.
        let starting = starting(dir);
        match look(dir, want.args.any_node, true) {
            Looked::Running(state, port) => return Outcome::Ready(up(want, state, port, None)),
            unusable @ (Looked::OtherNode(_) | Looked::Older(_)) => return Outcome::Unusable(unusable),
            Looked::Silent(state) if !starting => eprintln!("endeavor: the recorded runtime (pid {}) isn't answering; starting a new one", state.pid),
            _ => {}
        }
        if starting {
            drop(lock);
            match await_runtime(dir, want.args.any_node, None, hooks) {
                Ok((state, port)) => return Outcome::Ready(up(want, state, port, None)),
                // It died; the lock is taken again to look, and to start one if none is.
                Err(Waited::Gone) => continue,
                Err(Waited::Out(outcome)) => return outcome,
            }
        }
        stopped::clear(dir);
        let runtime_dir = match (want.runtime)() {
            Ok(dir) => dir,
            Err(e) => return Outcome::Failed(e),
        };
        let julia = match find_julia(&want.args.julia, want.engine, want.install, hooks) {
            Ok(julia) => julia,
            Err(outcome) => return outcome,
        };
        let token = match crate::token(dir) {
            Ok(token) => token,
            Err(e) => return Outcome::Failed(e),
        };
        let child = match crate::start(want.args, &runtime_dir, &julia, &token) {
            Ok(child) => child,
            Err(e) => return Outcome::Failed(e),
        };
        let runtime = Runtime::child(child, dir, want.events);
        if let Err(outcome) = await_core_lock(dir, &runtime) {
            return outcome;
        }
        drop(lock);
        return match await_runtime(dir, want.args.any_node, Some(&runtime), hooks) {
            Ok((state, port)) => Outcome::Ready(up(want, state, port, Some(runtime))),
            Err(Waited::Out(outcome)) => outcome,
            Err(Waited::Gone) => Outcome::Failed("The runtime ended while starting.".into()),
        };
    }
}

/// A runtime to use, the one `started` if it was started here.
fn up(want: &Want, state: State, port: u16, started: Option<Runtime>) -> Up {
    let was_started = started.is_some();
    let runtime = started.unwrap_or_else(|| Runtime::recorded(&state, &want.args.state_dir, want.events));
    Up { state, port, runtime, started: was_started }
}

/// Find Julia for the notebook system `engine`, or with `install` get it. The path of the julia that runs it.
pub(crate) fn find_julia(source: &julia::Source, engine: &str, install: bool, hooks: &mut dyn Hooks) -> Result<String, Outcome> {
    if engine != wire::ENGINE_PLUTO {
        return Err(Outcome::Failed(format!("Endeavor doesn't know a notebook system called \"{engine}\".")));
    }
    match julia::find(source, install, &mut |line| hooks.progress(line)) {
        Ok((path, version)) => {
            hooks.found(&version, &path);
            Ok(path)
        }
        Err(julia::Failure::Missing(item)) => Err(Outcome::NeedsInstall(vec![item])),
        Err(julia::Failure::Failed(message)) => Err(Outcome::Failed(message)),
    }
}

/// `DIR/starting.lock`, opened (it is made if there is none) and not yet held.
fn open_starting(dir: &Path) -> std::io::Result<File> {
    crate::owner_only(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)).open(dir.join("starting.lock"))
}

/// The core takes `starting.lock` before it does anything else, and holds it until it has recorded itself.
/// It is not inherited by Julia (files are opened close-on-exec): if it were, Julia would hold it for as
/// long as it runs.
pub(crate) fn hold_starting(dir: &Path) -> std::io::Result<File> {
    let file = open_starting(dir)?;
    file.lock()?;
    Ok(file)
}

/// Whether a core holds `starting.lock`: a start is under way. Asked with a shared lock, so that two who
/// ask never make each other see it held. On a home folder shared by several machines the lock may not
/// reach them all.
pub(crate) fn starting(dir: &Path) -> bool {
    open_starting(dir).is_ok_and(|file| file.try_lock_shared().is_err())
}

/// The core `runtime` was just spawned: wait until it holds `starting.lock`, so that nothing that takes
/// the start lock after this finds a spawned core it can't see.
fn await_core_lock(dir: &Path, runtime: &Runtime) -> Result<(), Outcome> {
    let until = Instant::now() + CORE_LOCK_WAIT;
    loop {
        if let Some(status) = runtime.exit.status() {
            let (status, log_tail) = runtime.died(status);
            return Err(Outcome::Died { status, log_tail });
        }
        if starting(dir) || crate::read_state(dir).is_some_and(|state| state.pid == runtime.pid) {
            return Ok(());
        }
        if Instant::now() > until {
            runtime.stop(None);
            return Err(Outcome::Failed(format!("Julia didn't start: the runtime (pid {}) did not take its lock in time. Its log is {}.", runtime.pid, dir.join("runtime.log").display())));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Why a wait for a runtime ended without one.
enum Waited {
    /// The start of another process ended with no runtime.
    Gone,
    Out(Outcome),
}

/// Wait for the runtime in `dir` to record itself and answer. `own` is the runtime this process
/// started: it is the only one `Hooks::wait` ending the wait stops, and the only one whose exit is
/// watched; for another process's start, the lock tells when it is over. The runtime's log goes to
/// `hooks` before the answer.
fn await_runtime(dir: &Path, any_node: bool, own: Option<&Runtime>, hooks: &mut dyn Hooks) -> Result<(State, u16), Waited> {
    let mut log = Log::new(dir.join("runtime.log"));
    let waiting = if own.is_some() { Waiting::Ready } else { Waiting::Other };
    let result = loop {
        log.drain(&mut |line| hooks.progress(line));
        if let Some(runtime) = own
            && let Some(status) = runtime.exit.status()
        {
            let (status, log_tail) = runtime.died(status);
            break Err(Waited::Out(Outcome::Died { status, log_tail }));
        }
        // Before the record is read: a core writes the record before it lets go of the lock.
        let held = own.is_some() || starting(dir);
        if let Looked::Running(state, port) = look(dir, any_node || own.is_some(), true)
            && own.is_none_or(|runtime| state.pid == runtime.pid)
        {
            break Ok((state, port));
        }
        if !held {
            break Err(Waited::Gone);
        }
        if !hooks.wait(POLL, waiting) {
            if let Some(runtime) = own {
                runtime.stop(None);
            }
            break Err(Waited::Out(Outcome::Cancelled));
        }
    };
    log.drain(&mut |line| hooks.progress(line));
    result
}

/// What ending the runtime recorded in a state folder came to.
pub(crate) enum Ended {
    /// None was recorded, or its process is gone.
    NotRunning,
    /// It runs on another machine (its name).
    Elsewhere(String),
    /// Stopped (its pid).
    Stopped(i32),
    /// Still running after the stop (its pid).
    Alive(i32),
    /// A core is starting and has no record yet: nothing was stopped.
    Starting,
}

/// What a stop says when a runtime is starting.
pub(crate) const STILL_STARTING: &str = "Julia was not stopped: it is still starting. Try again once it is up.";

/// End the runtime in `dir`, leaving a note of `how` for the clients still attached, and the record of one
/// whose process is gone removed. A runtime that is alive and not answering is stopped as well. The
/// caller holds the start lock, so no start is begun meanwhile; one that is under way is not touched.
pub(crate) fn end(dir: &Path, any_node: bool, how: stopped::How, events: &Sender<Event>) -> Ended {
    let starting = starting(dir);
    match look(dir, any_node, false) {
        Looked::OtherNode(state) => Ended::Elsewhere(state.node),
        Looked::Running(state, _) | Looked::Older(state) | Looked::Silent(state) => {
            if crate::stop_marked(dir, &state, &Runtime::recorded(&state, dir, events), how) { Ended::Stopped(state.pid) } else { Ended::Alive(state.pid) }
        }
        Looked::Dead(state) => {
            crate::remove_state(dir, state.pid);
            if starting { Ended::Starting } else { Ended::NotRunning }
        }
        Looked::NotRunning if starting => Ended::Starting,
        Looked::NotRunning => Ended::NotRunning,
    }
}

/// A log read line by line as it grows.
pub(crate) struct Log {
    path: PathBuf,
    lines: Option<BufReader<File>>,
    /// A line written in part, which waits for the rest.
    line: String,
}

impl Log {
    pub(crate) fn new(path: PathBuf) -> Log {
        Log { path, lines: None, line: String::new() }
    }

    /// Give `say` each whole line written since the last call, Pluto's secret masked.
    pub(crate) fn drain(&mut self, say: &mut dyn FnMut(String)) {
        if self.lines.is_none() {
            self.lines = File::open(&self.path).ok().map(BufReader::new);
        }
        let Some(lines) = &mut self.lines else { return };
        while let Ok(n) = lines.read_line(&mut self.line) {
            if n == 0 || !self.line.ends_with('\n') {
                return;
            }
            say(crate::redact_secret(self.line.trim_end()));
            self.line.clear();
        }
    }
}


#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn record(dir: &Path, node: &str, pid: i64, port: Option<u16>) {
        let mut state = serde_json::json!({ "launcher": "process", "node": node, "pid": pid, "token": "t" });
        if let Some(port) = port {
            state["port"] = port.into();
        }
        std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
    }

    /// A port that answers every call with 200.
    fn answering() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut socket in listener.incoming().flatten() {
                // The whole request first: answering and closing with part of it unread resets the connection.
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                while socket.read(&mut [0; 1024]).is_ok_and(|n| n > 0) {}
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        port
    }

    #[test]
    fn a_state_folder_is_running_not_running_or_cannot_be_used() {
        let dir = crate::client::scratch("look");
        let (here, me) = (crate::hostname(), std::process::id() as i64);
        assert!(matches!(look(&dir, false, true), Looked::NotRunning));

        record(&dir, &here, me, Some(answering()));
        assert!(matches!(look(&dir, false, true), Looked::Running(state, _) if state.pid as i64 == me));

        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        record(&dir, &here, me, Some(closed));
        assert!(matches!(look(&dir, false, true), Looked::Silent(_)));
        assert!(look(&dir, false, true).alive().is_some(), "a process that is alive is running, answering or not");
        assert!(matches!(look(&dir, false, false), Looked::Running(..)), "without a ping the port is not asked");

        record(&dir, &here, me, None);
        assert!(matches!(look(&dir, false, true), Looked::Older(_)));

        record(&dir, &here, i32::MAX as i64, Some(closed));
        assert!(matches!(look(&dir, false, true), Looked::Dead(state) if state.pid == i32::MAX), "its process is gone");

        record(&dir, "another-node", me, Some(answering()));
        assert!(matches!(look(&dir, false, true), Looked::OtherNode(state) if state.node == "another-node"));
        assert!(look(&dir, false, true).alive().is_none());
        assert!(matches!(look(&dir, true, true), Looked::Running(..)), "with any_node it is this machine's");
    }

    #[test]
    fn a_start_is_under_way_while_a_core_holds_its_lock_and_not_when_it_is_gone() {
        let dir = crate::client::scratch("starting");
        assert!(!starting(&dir));
        let core = hold_starting(&dir).unwrap();
        assert!(starting(&dir), "held");
        assert!(starting(&dir), "asking does not take it");
        drop(core);
        assert!(!starting(&dir), "a core that has recorded itself or died lets go");
    }

    #[test]
    fn ending_leaves_a_start_under_way_alone_and_clears_the_record_of_a_dead_runtime() {
        let dir = crate::client::scratch("end");
        let (events, _) = std::sync::mpsc::channel();
        let end = |any_node: bool| end(&dir, any_node, stopped::How::Stop, &events);
        assert!(matches!(end(false), Ended::NotRunning));

        let core = hold_starting(&dir).unwrap();
        assert!(matches!(end(false), Ended::Starting));
        record(&dir, &crate::hostname(), i32::MAX as i64, Some(1));
        assert!(matches!(end(false), Ended::Starting));
        assert!(!dir.join("runtime.json").exists(), "the record of a process that is gone is removed");
        drop(core);
        // The lock belongs to the open file description, which another test's forked child may still hold until it execs.
        let until = Instant::now() + Duration::from_secs(10);
        while starting(&dir) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(end(false), Ended::NotRunning));

        record(&dir, "another-node", std::process::id() as i64, Some(1));
        assert!(matches!(end(false), Ended::Elsewhere(node) if node == "another-node"));
        assert!(dir.join("runtime.json").exists(), "a record of another node's is left");
    }

    #[test]
    fn a_log_gives_whole_lines_as_they_are_written_with_the_secret_masked() {
        let dir = crate::client::scratch("log");
        let path = dir.join("runtime.log");
        let mut log = Log::new(path.clone());
        let mut lines = Vec::new();
        log.drain(&mut |line| lines.push(line));
        assert!(lines.is_empty(), "no log yet");
        std::fs::write(&path, "booting\nGo to http://localhost:1/?secret=abc now\npart").unwrap();
        log.drain(&mut |line| lines.push(line));
        assert_eq!(lines, ["booting", "Go to http://localhost:1/?secret=… now"]);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"ial\nnext\n").unwrap();
        log.drain(&mut |line| lines.push(line));
        assert_eq!(&lines[2..], ["partial", "next"]);
    }
}
