//! Finding the runtime in a state folder, or starting one: the one place that
//! does it, for the helper (`endeavor connect`), `serve` and `mcp`. The callers
//! differ in what they say while it goes on and how they end it (`Hooks`), and
//! in the words for what comes of it.
//!
//! A start that was begun finishes without the process that asked for it: the
//! core is its own session and records itself in `runtime.json` when Julia is
//! ready. Until then `starting.json` names it, so a client that comes meanwhile
//! waits for it instead of starting another. Only an explicit stop ends a start.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::{Args, Event, Runtime, State, julia, standalone, stopped};

/// What a state folder holds, found without starting or stopping anything.
pub(crate) enum Looked {
    NotRunning,
    /// Alive, and answering on its port.
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
            Looked::NotRunning | Looked::OtherNode(_) => None,
        }
    }
}

/// The runtime in `dir`. With `any_node` the record of another node is taken as this machine's:
/// the state folder belongs to this one machine, which was renamed.
pub(crate) fn look(dir: &Path, any_node: bool) -> Looked {
    let Some(state) = crate::read_state(dir) else { return Looked::NotRunning };
    if state.node != crate::hostname() && !any_node {
        return Looked::OtherNode(state);
    }
    if !crate::pid_alive(state.pid, state.started) {
        return Looked::NotRunning;
    }
    match state.port {
        None => Looked::Older(state),
        // Twice: a busy runtime can be slow to answer once.
        Some(port) if answers(port, &state.token) || answers(port, &state.token) => Looked::Running(state, port),
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
    /// Where a runtime started here reports its exit.
    pub events: &'a Sender<Event>,
}

/// What a caller says and decides while a start goes on.
pub(crate) trait Hooks {
    /// A line about how the start is going: Julia's download, the runtime's log.
    fn progress(&mut self, line: String);
    /// Julia was found, or installed.
    fn found(&mut self, version: &str, path: &str);
    /// Wait up to `wait`, and hear what the caller has to say meanwhile. False ends the start: for
    /// `Waiting::Ready` it stops the runtime, which is the one thing that does.
    fn wait(&mut self, wait: Duration, waiting: Waiting) -> bool;
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Waiting {
    /// For another process's start, which holds the lock.
    Lock,
    /// For the runtime to come up.
    Ready,
}

/// A runtime that can be used.
pub(crate) struct Up {
    pub state: State,
    pub port: u16,
    /// The runtime this call started, as its parent; none when it was there already.
    pub started: Option<Runtime>,
}

pub(crate) enum Unusable {
    /// It runs on this node of the home folder (its name).
    OtherNode(String),
    /// It is from a build before one port per runtime.
    Older,
}

pub(crate) enum Outcome {
    Ready(Up),
    /// The record can't be used, and no runtime was started.
    Unusable(Unusable),
    /// Julia is not there and `install` was false.
    NeedsInstall(Vec<wire::Item>),
    Failed(String),
    /// It exited while starting.
    Died { status: String, log_tail: Vec<String> },
    /// `Hooks::wait` said to stop; a runtime that was starting is stopped.
    Cancelled,
}

const POLL: Duration = Duration::from_millis(200);

/// Find the runtime in `want.args.state_dir`, or start one, and wait until it answers. The start lock is
/// held from before the first look until the runtime is up (or has failed), so callers asked at once
/// start one runtime and the rest use it.
pub(crate) fn find_or_start(want: &Want, hooks: &mut dyn Hooks) -> Outcome {
    let dir = &want.args.state_dir;
    if let Err(e) = crate::make_state_dir(dir) {
        return Outcome::Failed(e);
    }
    let waited = standalone::wait_for_start_lock(dir, standalone::start_lock_limit(), || if hooks.wait(POLL, Waiting::Lock) { Ok(()) } else { Err(()) });
    let _starting = match waited {
        Ok(lock) => lock,
        Err(standalone::Wait::Failed(message)) => return Outcome::Failed(message),
        Err(standalone::Wait::TimedOut) => return Outcome::Failed(standalone::start_lock_gave_up(dir)),
        Err(standalone::Wait::Interrupted(())) => return Outcome::Cancelled,
    };
    match look(dir, want.args.any_node) {
        Looked::Running(state, port) => return Outcome::Ready(Up { state, port, started: None }),
        Looked::OtherNode(state) => return Outcome::Unusable(Unusable::OtherNode(state.node)),
        Looked::Older(_) => return Outcome::Unusable(Unusable::Older),
        Looked::Silent(state) => eprintln!("endeavor: the recorded runtime (pid {}) isn't answering; starting a new one", state.pid),
        Looked::NotRunning => {}
    }
    if let Some((runtime, until)) = starting(dir, want.events) {
        return match await_runtime(dir, &runtime, hooks, Some(until)) {
            Ok((state, port)) => Outcome::Ready(Up { state, port, started: None }),
            Err(outcome) => outcome,
        };
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
    write_starting(dir, &runtime);
    match await_runtime(dir, &runtime, hooks, None) {
        Ok((state, port)) => Outcome::Ready(Up { state, port, started: Some(runtime) }),
        Err(outcome) => outcome,
    }
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

/// Follow the log of `runtime`, which is starting, until it records itself and answers on its port. The
/// log's lines go to `hooks` before the answer. A runtime that is not up is never left behind by this
/// function ending the start: only `Hooks::wait` saying to stop does that. `until` ends the wait.
fn await_runtime(dir: &Path, runtime: &Runtime, hooks: &mut dyn Hooks, until: Option<Instant>) -> Result<(State, u16), Outcome> {
    let mut log = Log::new(dir.join("runtime.log"));
    let result = loop {
        log.drain(&mut |line| hooks.progress(line));
        if let Some(status) = runtime.exit.status() {
            let (status, log_tail) = runtime.died(status);
            break Err(Outcome::Died { status, log_tail });
        }
        if let Some(state) = crate::read_state(dir)
            && state.pid == runtime.pid
            && let Some(port) = state.port
            && answers(port, &state.token)
        {
            break Ok((state, port));
        }
        if until.is_some_and(|until| Instant::now() > until) {
            break Err(Outcome::Failed(format!(
                "Gave up waiting for the Julia (pid {}) that another process started in {}. If none is starting, delete {} and try again.",
                runtime.pid,
                dir.display(),
                dir.join(STARTING).display()
            )));
        }
        if !hooks.wait(POLL, Waiting::Ready) {
            runtime.stop(None);
            break Err(Outcome::Cancelled);
        }
    };
    log.drain(&mut |line| hooks.progress(line));
    if result.is_ok() {
        clear_starting(dir, runtime.pid);
    }
    result
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

/// `DIR/starting.json`: the core a client started and has not yet seen ready, written under the start
/// lock. The core removes it when it records itself, and `remove_state` when it ends.
pub(crate) const STARTING: &str = "starting.json";

fn write_starting(dir: &Path, runtime: &Runtime) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let record = json!({ "pid": runtime.pid, "started": runtime.started_at(), "at": now });
    let _ = std::fs::write(dir.join(STARTING), record.to_string());
}

/// Remove the record of the start of `pid`, if it is the one named.
pub(crate) fn clear_starting(dir: &Path, pid: i32) {
    if read_starting(dir).is_some_and(|(recorded, _, _)| recorded == pid) {
        let _ = std::fs::remove_file(dir.join(STARTING));
    }
}

/// The pid, its start time and when it was started, as `write_starting` wrote them.
fn read_starting(dir: &Path) -> Option<(i32, Option<u64>, u64)> {
    let record: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(STARTING)).ok()?).ok()?;
    Some((i32::try_from(record["pid"].as_i64()?).ok()?, record["started"].as_u64(), record["at"].as_u64()?))
}

/// The core some earlier client started, which is still starting: it, and when to stop waiting for it
/// (a pid may be used again by another process). A record whose process is gone, or is too old, is removed.
pub(crate) fn starting(dir: &Path, events: &Sender<Event>) -> Option<(Runtime, Instant)> {
    let (pid, started, at) = read_starting(dir)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let left = Duration::from_secs(at).saturating_add(standalone::start_lock_limit()).saturating_sub(Duration::from_secs(now));
    if left.is_zero() || !crate::pid_alive(pid, started) {
        let _ = std::fs::remove_file(dir.join(STARTING));
        return None;
    }
    Some((Runtime::watching(pid, started, dir, events), Instant::now() + left))
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn record(dir: &Path, node: &str, pid: i64, port: Option<u16>) {
        let mut state = json!({ "launcher": "process", "node": node, "pid": pid, "token": "t" });
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
                let _ = socket.read(&mut [0; 1024]);
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        port
    }

    #[test]
    fn a_state_folder_is_running_not_running_or_cannot_be_used() {
        let dir = crate::client::scratch("look");
        let (here, me) = (crate::hostname(), std::process::id() as i64);
        assert!(matches!(look(&dir, false), Looked::NotRunning));

        record(&dir, &here, me, Some(answering()));
        assert!(matches!(look(&dir, false), Looked::Running(state, _) if state.pid as i64 == me));
        assert!(look(&dir, false).alive().is_some());

        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        record(&dir, &here, me, Some(closed));
        assert!(matches!(look(&dir, false), Looked::Silent(_)));
        assert!(look(&dir, false).alive().is_some(), "a process that is alive is running, answering or not");

        record(&dir, &here, me, None);
        assert!(matches!(look(&dir, false), Looked::Older(_)));

        record(&dir, &here, i32::MAX as i64, Some(closed));
        assert!(matches!(look(&dir, false), Looked::NotRunning), "its process is gone");

        record(&dir, "another-node", me, Some(answering()));
        assert!(matches!(look(&dir, false), Looked::OtherNode(state) if state.node == "another-node"));
        assert!(look(&dir, false).alive().is_none());
        assert!(matches!(look(&dir, true), Looked::Running(..)), "with any_node it is this machine's");
    }

    #[test]
    fn a_start_is_waited_for_only_while_its_process_is_alive_and_the_note_is_recent() {
        let dir = crate::client::scratch("starting");
        let (events, _) = std::sync::mpsc::channel();
        let me = std::process::id() as i32;
        let write = |pid: i32, at: u64| std::fs::write(dir.join(STARTING), json!({ "pid": pid, "started": null, "at": at }).to_string()).unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        assert!(starting(&dir, &events).is_none());
        write(me, now);
        assert!(starting(&dir, &events).is_some_and(|(runtime, _)| runtime.pid == me));
        clear_starting(&dir, me + 1);
        assert!(dir.join(STARTING).exists(), "another runtime's note stays");
        clear_starting(&dir, me);
        assert!(!dir.join(STARTING).exists());

        write(i32::MAX, now);
        assert!(starting(&dir, &events).is_none());
        assert!(!dir.join(STARTING).exists(), "a note whose process is gone is removed");

        write(me, now.saturating_sub(24 * 3600));
        assert!(starting(&dir, &events).is_none());
        assert!(!dir.join(STARTING).exists(), "so is one too old to trust");
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
