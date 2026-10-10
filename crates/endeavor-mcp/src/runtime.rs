//! Finding the runtime in a state folder, or starting one, and stopping it: the one place that does
//! each, for the helper (`endeavor connect`), `serve` and `mcp`. The callers differ in what they say
//! while it goes on and how they end it (`Hooks`), and in the words for what comes of it.
//!
//! A start, once begun, finishes without the process that asked for it. The core is its own session. As
//! the first thing it does it holds `starting.lock` in the state folder, and it lets go only after it has
//! written `runtime.json`; the OS lets go if it dies. So a held `starting.lock` with no usable record
//! means a start is under way, and one that is free with no record means there is none. The core writes
//! its pid and start time into the file once it holds the lock and blanks them before it lets go, so
//! whoever holds the lock is named while it is held. The process that spawned the core holds `start.lock`
//! until the core holds its own lock, and waits for the runtime without it. A client that waits for a
//! start never ends it; a forced stop (`end`) does, by the pid the file names.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
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
    if !crate::is_this_node(&state.node) && !any_node {
        return Looked::OtherNode(state);
    }
    if !crate::pid_alive(state.pid, state.started, state.boot.as_deref()) {
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

/// How long a runtime that is alive and doesn't answer is asked again before it counts as stuck: long
/// enough for a computer that just woke from sleep or a moment of heavy load, and well within the 45 s an
/// agent's call waits.
const SILENT_WAIT: Duration = Duration::from_secs(30);

/// `SILENT_WAIT`, or in a debug build `ENDEAVOR_TEST_SILENT_WAIT_SECS`.
pub(crate) fn silent_wait() -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_SILENT_WAIT_SECS").ok()?.parse().ok()).flatten();
    test.map_or(SILENT_WAIT, Duration::from_secs)
}

/// Look again, after each `pause`, at the runtime in `dir`, which was alive and didn't answer at `since`,
/// until it answers or goes, or `limit` has passed since then. None when `pause` says to stop.
pub(crate) fn ask_again(dir: &Path, any_node: bool, since: Instant, limit: Duration, pause: &mut dyn FnMut() -> bool) -> Option<Looked> {
    loop {
        if !pause() {
            return None;
        }
        let looked = look(dir, any_node, true);
        if !matches!(looked, Looked::Silent(_)) || since.elapsed() >= limit {
            return Some(looked);
        }
    }
}

/// Why no runtime is started beside one that is alive and doesn't answer.
pub(crate) fn silent_text(pid: i32) -> String {
    format!(
        "Julia here (pid {pid}) is running but hasn't answered for {} seconds, so no second one was started beside it. It may be busy, for example out of memory, or stuck. Stopping it (`endeavor stop`, or Stop in the Endeavor app) lets a new one start, and ends what it holds in memory.",
        silent_wait().as_secs()
    )
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
    /// Never start a runtime: find the one that runs, or wait for a start under way, and say nothing
    /// runs (`Outcome::NothingRunning`) when there is none or the start ended without one.
    pub attach_only: bool,
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
    /// For a runtime that is alive and silent to answer again.
    Silent,
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
    /// `Want::attach_only`, and no runtime runs and none is starting.
    NothingRunning,
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
        let looked_at = Instant::now();
        // Before the look: a core that records itself and lets go in between is then found by the look.
        let starting = lock_state(dir).is_held();
        match look(dir, want.args.any_node, true) {
            Looked::Running(state, port) => return Outcome::Ready(up(want, state, port, None)),
            unusable @ (Looked::OtherNode(_) | Looked::Older(_)) => return Outcome::Unusable(unusable),
            // Never a second runtime beside one that is alive: both would open the same notebooks.
            Looked::Silent(state) if !starting && !want.attach_only => {
                drop(lock);
                hooks.progress(format!("Julia here (pid {}) isn't answering; asking it again for up to {} seconds.", state.pid, silent_wait().as_secs()));
                match ask_again(dir, want.args.any_node, looked_at, silent_wait(), &mut || hooks.wait(POLL, Waiting::Silent)) {
                    None => return Outcome::Cancelled,
                    Some(Looked::Silent(state)) => return Outcome::Failed(silent_text(state.pid)),
                    // It answered or went: looked at again with the lock.
                    Some(_) => continue,
                }
            }
            // Its core was killed: what it started may still run, and open the same notebooks as the new one.
            Looked::Dead(state) if !starting && !want.attach_only && crate::stop_workers(state.pid, state.started, state.boot.as_deref()) => {
                hooks.progress(format!("Stopped what the last Julia here left running when it ended (pid {}).", state.pid));
            }
            _ => {}
        }
        if starting {
            drop(lock);
            match await_runtime(dir, want.args.any_node, None, hooks) {
                Ok((state, port)) => return Outcome::Ready(up(want, state, port, None)),
                // It died; the lock is taken again to look, and to start one if none is, unless none is to be started.
                Err(Waited::Gone) if want.attach_only => return Outcome::NothingRunning,
                Err(Waited::Gone) => continue,
                Err(Waited::Out(outcome)) => return outcome,
            }
        }
        if want.attach_only {
            return Outcome::NothingRunning;
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

/// The process that holds `starting.lock`, as the file names it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Core {
    pub pid: i32,
    /// As in `State`.
    pub started: Option<u64>,
    pub boot: Option<String>,
    /// The computer it runs on: a pid means nothing on another.
    pub node: String,
}

impl Core {
    /// The core is this process.
    fn this() -> Core {
        Core { pid: std::process::id() as i32, started: crate::own_start_time(), boot: crate::own_boot(), node: crate::hostname() }
    }
}

/// Where on Windows the core's lock is: a byte past what the file holds (`write_core`), so that
/// reading the file does not need the lock. The shared lock `starting` asks for covers the whole file.
#[cfg(windows)]
const LOCKED_BYTE: u32 = 4096;

#[cfg(unix)]
fn lock_starting(file: &File) -> std::io::Result<()> {
    file.lock()
}

/// On Windows a byte-range lock is mandatory: the lock `File::lock` takes covers every byte, and another
/// process could not read the pid the file holds.
#[cfg(windows)]
fn lock_starting(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut at = OVERLAPPED::default();
    at.Anonymous.Anonymous.Offset = LOCKED_BYTE;
    // SAFETY: a handle we own, one byte, and an OVERLAPPED that lives until the call returns (it waits for the lock).
    let locked = unsafe { LockFileEx(file.as_raw_handle() as HANDLE, LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &mut at) };
    if locked == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Write who holds the lock (or, with none, that no one does) as the whole of what the file holds. The
/// lock is held when this is called, so no shared lock over the file (`lock_state`) is held meanwhile on
/// Windows.
fn write_core(mut file: &File, core: Option<&Core>) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    match core {
        Some(core) => file.write_all(serde_json::json!({ "pid": core.pid, "started": core.started, "boot": core.boot, "node": core.node }).to_string().as_bytes()),
        None => Ok(()),
    }
}

/// What the file says, read as it is: whether the process is there is for `lock_state`.
fn read_core(mut file: &File) -> Option<Core> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(512).read_to_string(&mut text).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(Core {
        pid: v["pid"].as_i64().and_then(|pid| i32::try_from(pid).ok()).filter(|pid| *pid > 0)?,
        started: v["started"].as_u64(),
        boot: v["boot"].as_str().map(str::to_owned),
        node: v["node"].as_str()?.to_owned(),
    })
}

/// The core takes `starting.lock` before it does anything else, writes its own pid and start time in it,
/// and holds it until it has recorded itself (`release_starting`). It is not inherited by Julia (files are
/// opened close-on-exec): if it were, Julia would hold it for as long as it runs.
pub(crate) fn hold_starting(dir: &Path) -> std::io::Result<File> {
    let file = open_starting(dir)?;
    lock_starting(&file)?;
    write_core(&file, Some(&Core::this())).map_err(|e| std::io::Error::new(e.kind(), format!("couldn't write its pid in starting.lock: {e}")))?;
    Ok(file)
}

/// Let go of `starting.lock` once `runtime.json` is written. The pid goes first: a file that still named
/// this process once a later core holds the lock would have a forced stop end a runtime that is up.
pub(crate) fn release_starting(file: File) {
    let _ = write_core(&file, None);
}

/// What `starting.lock` says.
#[derive(Debug, PartialEq)]
pub(crate) enum Lock {
    /// No core holds it: no start is under way.
    Free,
    /// A core holds it: a start is under way. The core the file names, when it names a process on this
    /// computer that is the one named (`pid_alive`) and has no record, so it has not finished; none for a
    /// core of an older build, one that has just taken the lock, or one that is another computer's.
    Held(Option<Core>),
}

impl Lock {
    pub(crate) fn is_held(&self) -> bool {
        matches!(self, Lock::Held(_))
    }

    /// The core the file names.
    pub(crate) fn core(&self) -> Option<&Core> {
        match self {
            Lock::Held(core) => core.as_ref(),
            Lock::Free => None,
        }
    }
}

/// The one reading of `starting.lock`: whether a core holds it, and who. Asked with a shared lock, so that
/// two who ask never make each other see it held. On a home folder shared by several machines the lock may
/// not reach them all. It makes no file: a core makes `starting.lock` before it holds it, so none means no
/// start. Read before the record is, since a core writes the record before it lets go.
pub(crate) fn lock_state(dir: &Path) -> Lock {
    let Ok(file) = File::open(dir.join("starting.lock")) else { return Lock::Free };
    if file.try_lock_shared().is_ok() {
        return Lock::Free;
    }
    let core = read_core(&file).filter(|core| {
        crate::is_this_node(&core.node)
            && crate::pid_alive(core.pid, core.started, core.boot.as_deref())
            && !crate::read_state(dir).is_some_and(|state| state.pid == core.pid)
    });
    Lock::Held(core)
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
        // Named in the file, so that whoever takes the start lock after this finds it ready to be stopped.
        if lock_state(dir).core().is_some_and(|core| core.pid == runtime.pid) || crate::read_state(dir).is_some_and(|state| state.pid == runtime.pid) {
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
    // The core of a start another process owns, to tell whether it ended because it was stopped.
    let mut core = None;
    let result = loop {
        log.drain(&mut |line| hooks.progress(line));
        if let Some(runtime) = own
            && let Some(status) = runtime.exit.status()
        {
            let (status, log_tail) = runtime.died(status);
            break Err(Waited::Out(Outcome::Died { status, log_tail }));
        }
        // Before the record is read: a core writes the record before it lets go of the lock.
        let lock = lock_state(dir);
        let held = own.is_some() || lock.is_held();
        if own.is_none() {
            core = lock.core().map(|core| core.pid).or(core);
        }
        if let Looked::Running(state, port) = look(dir, any_node || own.is_some(), true)
            && own.is_none_or(|runtime| state.pid == runtime.pid)
        {
            break Ok((state, port));
        }
        if !held {
            // A start that was stopped is not started again for whoever waited for it.
            break Err(match core.and_then(|pid| stopped::why(dir, stopped::Of::Runtime(pid))) {
                Some(how) => Waited::Out(Outcome::Failed(format!("Julia was stopped while it was starting. {}", crate::stopped_text(how)))),
                None => Waited::Gone,
            });
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
    /// A core is starting and has no record yet, and `force` was not given: nothing was stopped.
    Starting,
    /// `force` was given, and the file does not name the core that holds `starting.lock`, or what holds
    /// it changed meanwhile: nothing was stopped.
    Unidentified,
    /// A start that was under way was cancelled (its core's pid).
    Cancelled(i32),
}

/// What a helper's stop says when a runtime is starting and its `Stop` was not forced.
pub(crate) const STILL_STARTING: &str = "Julia was not stopped: it is still starting. Try again once it is up, or force the stop to cancel the start.";

/// What a stop here says when a runtime is starting and `force` was not given.
pub(crate) const STILL_STARTING_FORCE: &str = "Julia was not stopped: it is still starting. A stop with `force` cancels the start.";

/// What a forced stop says when the process that is starting Julia can't be told.
pub(crate) const START_UNIDENTIFIED: &str = "Julia is starting, but Endeavor can't tell which process is starting it, so nothing was stopped. Try again in a moment. If this goes on, find the process with `ps` and end it.";

/// End the runtime in `dir`, leaving a note of `how` for the clients still attached, and the record of one
/// whose process is gone removed. A runtime that is alive and not answering is stopped as well. The
/// caller holds the start lock, so no start is begun meanwhile; one that is under way is not touched,
/// unless `force` is given: then its core is ended, with Julia and its children, and nothing of it is left.
pub(crate) fn end(dir: &Path, any_node: bool, how: stopped::How, force: bool, events: &Sender<Event>) -> Ended {
    // Before the look: a core that records itself and lets go in between is then found by the look.
    let lock = lock_state(dir);
    match look(dir, any_node, false) {
        Looked::OtherNode(state) => Ended::Elsewhere(state.node),
        Looked::Running(state, _) | Looked::Older(state) | Looked::Silent(state) => stop_up(dir, state, how, events),
        Looked::Dead(state) => {
            crate::stop_workers(state.pid, state.started, state.boot.as_deref());
            crate::remove_state(dir, state.pid, state.started);
            without_runtime(dir, any_node, lock, how, force, events)
        }
        Looked::NotRunning => without_runtime(dir, any_node, lock, how, force, events),
    }
}

/// Stop the runtime `state` records, which is alive.
fn stop_up(dir: &Path, state: State, how: stopped::How, events: &Sender<Event>) -> Ended {
    if crate::stop_marked(dir, Some(&state), &Runtime::recorded(&state, dir, events), how) { Ended::Stopped(state.pid) } else { Ended::Alive(state.pid) }
}

/// How long the lock of a core that was ended is waited for. The OS lets it go when the core's last
/// handle closes, and a process the core was starting may hold the file a moment longer.
const LOCK_FREED_WITHIN: Duration = Duration::from_secs(5);

/// No runtime is recorded as alive, and `lock` was read before the look. With a start under way and
/// `force`, end the core the lock file names: the same stop as a runtime that is up (`stop_marked`), with
/// its note for the clients that wait. The lock is read again immediately before the stop, and the first
/// signal goes only to a process with the start time and boot the file gives (`Runtime::kill` checks
/// them before each signal). A core is ended only if the file gives a start time, and the computer it
/// runs on is this one; otherwise nothing is signalled, after a last look for the record of a start that
/// has finished meanwhile, which is then stopped as any runtime is.
fn without_runtime(dir: &Path, any_node: bool, lock: Lock, how: stopped::How, force: bool, events: &Sender<Event>) -> Ended {
    if !lock.is_held() {
        return Ended::NotRunning;
    }
    if !force {
        return Ended::Starting;
    }
    if let Lock::Held(Some(core)) = lock_state(dir)
        && core.started.is_some()
    {
        let runtime = Runtime::of(core.pid, core.started, core.boot.clone(), dir, events);
        if !crate::stop_marked(dir, None, &runtime, how) {
            return Ended::Alive(core.pid);
        }
        // What the core would have removed.
        for name in ["runtime.json.tmp", "julia.json"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let until = Instant::now() + LOCK_FREED_WITHIN;
        while lock_state(dir).is_held() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        return Ended::Cancelled(core.pid);
    }
    match look(dir, any_node, false) {
        Looked::Running(state, _) | Looked::Older(state) | Looked::Silent(state) => stop_up(dir, state, how, events),
        _ if lock_state(dir).is_held() => Ended::Unidentified,
        _ => Ended::NotRunning,
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
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn starting(dir: &Path) -> bool {
        lock_state(dir).is_held()
    }

    /// A process of ours that does nothing for a minute, in its own group.
    struct Sleeper(Child);

    impl Sleeper {
        fn new() -> Sleeper {
            use std::os::unix::process::CommandExt;
            Sleeper(Command::new("sleep").arg("60").process_group(0).stdin(Stdio::null()).spawn().unwrap())
        }

        fn pid(&self) -> i32 {
            self.0.id() as i32
        }

        fn started(&self) -> u64 {
            crate::unixproc::start_time(self.pid()).at().expect("a start time on this platform")
        }

        /// The core file would name this process, on this computer, as it is.
        fn core(&self) -> Core {
            Core { pid: self.pid(), started: Some(self.started()), boot: crate::own_boot(), node: crate::hostname() }
        }

        fn alive(&mut self) -> bool {
            self.0.try_wait().unwrap().is_none()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn record(dir: &Path, node: &str, pid: i64, port: Option<u16>) {
        let mut state = serde_json::json!({ "launcher": "process", "node": node, "pid": pid, "token": "t" });
        if let Some(port) = port {
            state["port"] = port.into();
        }
        std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
    }

    /// A port that answers every call with 200.
    fn answering() -> u16 {
        counting().0
    }

    /// A port that answers every call with 200, and how many connections it took.
    fn counting() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let taken = Arc::new(AtomicUsize::new(0));
        let counted = taken.clone();
        std::thread::spawn(move || {
            for mut socket in listener.incoming().flatten() {
                counted.fetch_add(1, Ordering::SeqCst);
                // The whole request first: answering and closing with part of it unread resets the connection.
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                while socket.read(&mut [0; 1024]).is_ok_and(|n| n > 0) {}
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        (port, taken)
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

    #[cfg(windows)]
    #[test]
    fn on_windows_a_node_recorded_in_another_case_is_this_computer() {
        let dir = crate::client::scratch("look-case");
        let me = std::process::id() as i64;
        let here = crate::hostname();
        let other_case: String = here.chars().map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() }).collect();
        assert_ne!(other_case, here, "the host name has letters");
        record(&dir, &other_case, me, Some(answering()));
        assert!(matches!(look(&dir, false, true), Looked::Running(..)), "{other_case} is {here}");
    }

    #[test]
    fn a_start_is_under_way_while_a_core_holds_its_lock_and_not_when_it_is_gone() {
        let dir = crate::client::scratch("starting");
        assert!(!starting(&dir));
        let core = hold_starting(&dir).unwrap();
        assert!(starting(&dir), "held");
        assert!(starting(&dir), "asking does not take it");
        drop(core);
        wait_free(&dir);
        assert!(!starting(&dir), "a core that has recorded itself or died lets go");
    }

    #[test]
    fn ending_leaves_a_start_under_way_alone_and_clears_the_record_of_a_dead_runtime() {
        let dir = crate::client::scratch("end");
        let (events, _) = std::sync::mpsc::channel();
        let end = |any_node: bool| end(&dir, any_node, stopped::How::Stop, false, &events);
        assert!(matches!(end(false), Ended::NotRunning));

        let core = hold_starting(&dir).unwrap();
        assert!(matches!(end(false), Ended::Starting));
        record(&dir, &crate::hostname(), i32::MAX as i64, Some(1));
        assert!(matches!(end(false), Ended::Starting));
        assert!(!dir.join("runtime.json").exists(), "the record of a process that is gone is removed");
        drop(core);
        // The lock belongs to the open file description, which another test's forked child may still hold until it execs.
        wait_free(&dir);
        assert!(matches!(end(false), Ended::NotRunning));

        record(&dir, "another-node", std::process::id() as i64, Some(1));
        assert!(matches!(end(false), Ended::Elsewhere(node) if node == "another-node"));
        assert!(dir.join("runtime.json").exists(), "a record of another node's is left");
    }

    /// Until nothing holds `starting.lock`: a process another test forks holds its descriptor until it execs.
    fn wait_free(dir: &Path) {
        let until = Instant::now() + Duration::from_secs(10);
        while starting(dir) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn record_started(dir: &Path, pid: i32, started: Option<u64>, port: u16) {
        record_booted(dir, pid, started, crate::own_boot(), port);
    }

    fn record_booted(dir: &Path, pid: i32, started: Option<u64>, boot: Option<String>, port: u16) {
        let state = serde_json::json!({ "launcher": "process", "node": crate::hostname(), "pid": pid, "started": started, "boot": boot, "token": "t", "port": port });
        std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
    }

    #[test]
    fn a_recorded_pid_that_is_another_process_now_is_not_a_runtime_and_is_never_pinged_or_signalled() {
        let dir = crate::client::scratch("pid-reused");
        let (port, pings) = counting();
        let mut other = Sleeper::new();
        let right = other.started();

        record_started(&dir, other.pid(), Some(right), port);
        assert!(matches!(look(&dir, false, true), Looked::Running(state, _) if state.pid == other.pid()), "the process that started then");
        assert_eq!(pings.load(Ordering::SeqCst), 1);

        record_started(&dir, other.pid(), None, port);
        assert!(matches!(look(&dir, false, true), Looked::Running(..)), "a record from a build that wrote no start time is trusted as before");
        assert_eq!(pings.load(Ordering::SeqCst), 2);

        // A reboot: the pid is alive, and is another program's.
        record_started(&dir, other.pid(), Some(right + 1), port);
        assert!(matches!(look(&dir, false, true), Looked::Dead(state) if state.pid == other.pid()));
        assert!(look(&dir, false, true).alive().is_none());
        assert_eq!(pings.load(Ordering::SeqCst), 2, "its port was not asked, so the token was not sent");

        // The same start time on another boot (Linux counts it from boot) is not the same process.
        if crate::unixproc::boot_id().is_some() {
            record_booted(&dir, other.pid(), Some(right), Some("another-boot".into()), port);
            assert!(matches!(look(&dir, false, true), Looked::Dead(_)));
            assert_eq!(pings.load(Ordering::SeqCst), 2);
            record_booted(&dir, other.pid(), Some(right), None, port);
            assert!(matches!(look(&dir, false, true), Looked::Running(..)), "a record without a boot id is compared as before");
            assert_eq!(pings.load(Ordering::SeqCst), 3);
            record_started(&dir, other.pid(), Some(right + 1), port);
        }

        let (events, _) = std::sync::mpsc::channel();
        assert!(matches!(end(&dir, false, stopped::How::Stop, true, &events), Ended::NotRunning));
        assert!(!dir.join("runtime.json").exists(), "the stale record is removed");
        assert!(!dir.join("stopped").exists() && other.alive(), "nothing was signalled, though a forced stop was asked for");

        // The same through the process that ends a runtime: a pid that is not the one recorded is left alone.
        record_started(&dir, other.pid(), Some(right + 1), port);
        let runtime = Runtime::of(other.pid(), Some(right + 1), crate::own_boot(), &dir, &events);
        runtime.kill();
        assert!(other.alive(), "not signalled");
        assert!(!dir.join("runtime.json").exists(), "and the record of a process that isn't it is removed");
        record_started(&dir, other.pid(), Some(right), port);
        // Our child is not gone until it is reaped, which `alive` does.
        let runtime = Runtime::of(other.pid(), Some(right), crate::own_boot(), &dir, &events);
        let until = Instant::now() + Duration::from_secs(20);
        std::thread::scope(|scope| {
            scope.spawn(|| runtime.kill());
            while other.alive() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        assert!(!other.alive(), "the process that is recorded is ended");
    }

    #[test]
    fn a_pid_counts_unless_it_is_no_process_or_started_at_another_time() {
        let me = std::process::id() as i32;
        let started = crate::own_start_time();
        let boot = crate::own_boot();
        assert!(crate::pid_alive(me, started, boot.as_deref()));
        assert!(crate::pid_alive(me, None, None), "no start time recorded: as before");
        assert!(!crate::pid_alive(me, started.map(|started| started + 1), boot.as_deref()), "another start time");
        assert!(!crate::pid_alive(i32::MAX, started, None), "no such process");
        assert!(!crate::pid_alive(0, None, None) && !crate::pid_alive(-1, None, None));
        if boot.is_some() {
            assert!(!crate::pid_alive(me, started, Some("another-boot")), "another boot");
        }
    }

    #[test]
    fn a_core_that_holds_starting_lock_is_named_in_it_until_it_lets_go() {
        let dir = crate::client::scratch("core-named");
        assert_eq!(lock_state(&dir), Lock::Free, "no file");
        let me = Core::this();
        let core = hold_starting(&dir).unwrap();
        assert_eq!(lock_state(&dir), Lock::Held(Some(me.clone())));
        assert_eq!(lock_state(&dir), Lock::Held(Some(me.clone())), "asking does not take it");

        record_started(&dir, me.pid, me.started, 1);
        assert_eq!(lock_state(&dir), Lock::Held(None), "a core with a record has finished");
        let _ = std::fs::remove_file(dir.join("runtime.json"));
        assert_eq!(lock_state(&dir), Lock::Held(Some(me.clone())));

        release_starting(core);
        wait_free(&dir);
        assert_eq!(lock_state(&dir), Lock::Free);
        assert_eq!(read_core(&File::open(dir.join("starting.lock")).unwrap()), None, "the pid is blanked");

        // A core that dies without letting go leaves its pid in the file and no lock.
        let core = hold_starting(&dir).unwrap();
        drop(core);
        wait_free(&dir);
        assert_eq!(lock_state(&dir), Lock::Free);
    }

    #[test]
    fn the_file_names_the_core_only_if_that_process_is_the_one_that_started_then_on_this_computer() {
        let dir = crate::client::scratch("core-identity");
        let mut other = Sleeper::new();
        let lock = open_starting(&dir).unwrap();
        lock.lock().unwrap();
        let says = |core: Core| {
            write_core(&lock, Some(&core)).unwrap();
            lock_state(&dir)
        };
        let named = other.core();
        assert_eq!(says(named.clone()), Lock::Held(Some(named.clone())));
        let no_time = Core { started: None, ..named.clone() };
        assert_eq!(says(no_time.clone()), Lock::Held(Some(no_time)), "no start time recorded: as before");
        assert_eq!(says(Core { started: named.started.map(|started| started + 1), ..named.clone() }), Lock::Held(None), "another process with that pid");
        assert_eq!(says(Core { node: "another-node".into(), ..named.clone() }), Lock::Held(None), "another computer's pid number means nothing here");
        if named.boot.is_some() {
            assert_eq!(says(Core { boot: Some("another-boot".into()), ..named.clone() }), Lock::Held(None));
        }
        assert_eq!(says(Core { pid: i32::MAX, started: Some(1), ..named.clone() }), Lock::Held(None), "no such process");
        other.0.kill().unwrap();
        other.0.wait().unwrap();
        assert_eq!(says(named), Lock::Held(None), "gone");
        write_core(&lock, None).unwrap();
        assert_eq!(lock_state(&dir), Lock::Held(None), "a core that names no one");
    }

    #[test]
    fn a_forced_stop_ends_nothing_unless_the_file_gives_the_core_and_its_start_time() {
        let dir = crate::client::scratch("force-unnamed");
        let (events, _) = std::sync::mpsc::channel();
        let mut other = Sleeper::new();
        let lock = open_starting(&dir).unwrap();
        lock.lock().unwrap();
        let force = || end(&dir, false, stopped::How::Stop, true, &events);
        // A core of an older build writes no pid.
        assert!(matches!(force(), Ended::Unidentified));
        assert!(matches!(end(&dir, false, stopped::How::Stop, false, &events), Ended::Starting));
        // A pid that is another process now.
        write_core(&lock, Some(&Core { started: Some(other.started() + 1), ..other.core() })).unwrap();
        assert!(matches!(force(), Ended::Unidentified));
        // The process is there, but the file gives no start time: it is never signalled on the pid alone.
        write_core(&lock, Some(&Core { started: None, ..other.core() })).unwrap();
        assert!(matches!(force(), Ended::Unidentified));
        // A pid of another computer.
        write_core(&lock, Some(&Core { node: "another-node".into(), ..other.core() })).unwrap();
        assert!(matches!(force(), Ended::Unidentified));
        assert!(other.alive() && !dir.join("stopped").exists(), "nothing was signalled");
        // A named process, but no one holds the lock: no start is under way.
        write_core(&lock, Some(&other.core())).unwrap();
        drop(lock);
        wait_free(&dir);
        assert!(matches!(force(), Ended::NotRunning));
        assert!(other.alive() && !dir.join("stopped").exists(), "nothing was signalled");
    }

    #[test]
    fn a_forced_stop_that_finds_the_start_finished_stops_the_runtime_instead_of_saying_it_cannot_tell() {
        let dir = crate::client::scratch("force-finished");
        let (events, _) = std::sync::mpsc::channel();
        let mut other = Sleeper::new();
        let lock = open_starting(&dir).unwrap();
        lock.lock().unwrap();
        // The lock was read as held with no core named; the core has recorded itself since.
        // Without a port, so that the stop does not wait for the runtime to answer a shutdown.
        let state = serde_json::json!({ "launcher": "process", "node": crate::hostname(), "pid": other.pid(), "started": other.started(), "boot": crate::own_boot(), "token": "t" });
        std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
        let ended = std::thread::scope(|scope| {
            let stopping = scope.spawn(|| without_runtime(&dir, false, Lock::Held(None), stopped::How::Stop, true, &events));
            let until = Instant::now() + Duration::from_secs(20);
            while other.alive() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            stopping.join().unwrap()
        });
        assert!(matches!(ended, Ended::Stopped(pid) if pid == other.pid()), "the runtime that is up is stopped as usual");
        assert!(!other.alive());
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
