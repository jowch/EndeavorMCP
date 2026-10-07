//! `client::Session` against the helper binary, with a local `sh` standing in
//! for ssh (`Transport::Shell`) and a stand-in Julia under the real core, so
//! neither sshd nor Julia is needed: what `ensure` answers, getting the
//! connection back on the same port, closing, and two sessions in one process.
//! Every folder is under `target/tmp`.

#![cfg(unix)]

mod common;

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{FakeBridge, TOKEN, pid_alive, serving_julia, wait_for};
use endeavor_mcp::client::{Config, Outcome, RuntimeInfo, Server, Session, State, Transport, Want};

/// One machine called `lab-NAME` whose helper and runtime live in a folder of the test's own.
struct Place {
    dir: PathBuf,
    server: Server,
    /// The helper's state folder, where its runtime keeps `runtime.json`.
    state: PathBuf,
    _bridge: FakeBridge,
}

impl Place {
    fn new(name: &str) -> Place {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("session-{name}"));
        end_runtime(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("runtime-state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let bridge = FakeBridge::start(&state);
        let julia = serving_julia(&state, &bridge);
        std::fs::write(state.join("token"), TOKEN).unwrap();
        let server = Server { id: format!("lab-{name}"), name: "lab".into(), ssh_host: "lab".into(), julia: Some(julia.display().to_string()), ..Default::default() };
        Place { dir, server, state, _bridge: bridge }
    }

    /// A session for the machine. `install`: the user agreed to the helper. `ask` runs before each sign-in.
    fn session_with(&self, install: bool, ask: Option<String>) -> Session {
        let path = |name: &str| self.dir.join(name).display().to_string();
        let mut config = Config::new(self.server.clone(), |_, _| Ok(PathBuf::from(env!("CARGO_BIN_EXE_endeavor"))));
        let env = [("HOME", path("home")), ("XDG_STATE_HOME", path("state-home")), ("XDG_CONFIG_HOME", path("config")), ("XDG_CACHE_HOME", path("cache"))];
        config.transport = Transport::Shell { env: env.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(), ask };
        (config.root, config.state, config.depot, config.allow_install) = (path("root"), self.state.display().to_string(), path("depot"), install);
        Session::new(config).expect("a session")
    }

    fn session(&self) -> Session {
        self.session_with(true, None)
    }

    /// The pids of the helpers (`endeavor connect`) of this machine.
    fn helpers(&self) -> Vec<i32> {
        let found = Command::new("pgrep").arg("-f").arg("--").arg(format!("connect --state-dir {}", self.state.display())).output().unwrap();
        String::from_utf8_lossy(&found.stdout).split_whitespace().filter_map(|p| p.parse().ok()).collect()
    }

    fn julia_ran(&self) -> bool {
        self.state.join("julia.args").exists()
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        end_runtime(&self.dir);
    }
}

/// End the runtime a test of `dir` started (the core, Julia and its workers are one process group).
fn end_runtime(dir: &Path) {
    let recorded = std::fs::read_to_string(dir.join("runtime-state/runtime.json")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    if let Some(pid) = recorded.and_then(|v| v["pid"].as_i64()).filter(|&p| p > 1) {
        // SAFETY: plain syscall, on the runtime this test started.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    }
}

const LONG: Duration = Duration::from_secs(40);

fn start() -> Want {
    Want::Start { job: None, install: true }
}

fn ready(outcome: Outcome) -> RuntimeInfo {
    match outcome {
        Outcome::Ready(runtime) => runtime,
        other => panic!("not ready: {other:?}"),
    }
}

/// Whether anything takes a connection on `port`.
fn listening(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// Whether the process has a thread called `name`.
#[cfg(target_os = "linux")]
fn has_thread(name: &str) -> bool {
    std::fs::read_dir("/proc/self/task").unwrap().flatten().any(|task| std::fs::read_to_string(task.path().join("comm")).is_ok_and(|comm| comm.trim() == name))
}

#[test]
fn ensure_answers_ready_with_the_listeners_port_and_again_the_same() {
    let place = Place::new("ready");
    let session = place.session();
    let runtime = ready(session.ensure(start(), LONG));
    assert_eq!(runtime.port, session.port(), "the runtime is reached through the listener");
    assert!(runtime.page_url.contains(&format!(":{}/", runtime.port)) && runtime.mcp_url.ends_with("/mcp"), "{runtime:?}");
    assert!(pid_alive(runtime.pid as i32) && !runtime.reattached && runtime.token.len() == 64);
    assert!(listening(runtime.port));
    assert_eq!(ready(session.ensure(start(), Duration::ZERO)), runtime, "asked again, it is the same");
    let status = session.status();
    assert_eq!((status.state, status.machine.as_str(), status.name.as_str()), (State::Ready, "lab-ready", "lab"));
    assert_eq!(status.runtime, Some(runtime));
    assert_eq!(status.hello.map(|h| h.helper_installed), Some(Some(true)));
}

#[test]
fn an_attach_with_nothing_running_says_so_and_a_start_after_it_starts() {
    let place = Place::new("attach");
    let session = place.session();
    assert_eq!(session.ensure(Want::Attach { install: false }, LONG), Outcome::NothingRunning);
    assert!(!place.julia_ran(), "no Julia was started");
    assert!(session.status().nothing_running);
    let runtime = ready(session.ensure(start(), LONG));
    assert!(!session.status().nothing_running, "a start clears it");
    // Another session attaches to what runs, and is not told that nothing does.
    drop(session);
    let second = place.session();
    let attached = ready(second.ensure(Want::Attach { install: false }, LONG));
    assert!(attached.reattached && attached.pid == runtime.pid, "{attached:?}");
}

#[test]
fn a_machine_without_the_helper_needs_the_agreement_and_the_agreement_installs_it() {
    let place = Place::new("needs-install");
    let session = place.session_with(false, None);
    let Outcome::NeedsInstall(needs) = session.ensure(Want::Start { job: None, install: false }, LONG) else { panic!("it needs the helper") };
    assert_eq!(needs.items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(), [wire::KIND_HELPER]);
    assert!(needs.needs_helper());
    assert!(!place.dir.join("root").exists() && place.helpers().is_empty(), "nothing was installed");
    // Asked again with the agreement, it installs and starts.
    ready(session.ensure(start(), LONG));
    assert!(place.dir.join("root").exists());
    assert_eq!(session.status().needs_install, None);
}

#[test]
fn a_failed_sign_in_is_told_once_and_the_call_after_that_tries_again() {
    let place = Place::new("failed");
    let asked = place.dir.join("asked");
    std::fs::create_dir_all(&place.dir).unwrap();
    let ask = format!("echo x >> {}; echo 'jc@lab: Permission denied (publickey).' >&2; exit 255", asked.display());
    let attempts = || std::fs::read_to_string(&asked).map_or(0, |text| text.lines().count());
    let session = place.session_with(true, Some(ask));
    wait_for("the first attempt to fail", || attempts() == 1 && session.status().state == State::Failed);
    // Asked now, it connects again, and the answer is not in yet.
    assert!(matches!(session.ensure(start(), Duration::ZERO), Outcome::StillWorking(_)));
    wait_for("the second attempt to fail", || attempts() == 2 && session.status().state == State::Failed);
    let Outcome::Failed(why) = session.ensure(start(), LONG) else { panic!("it failed") };
    assert!(why.contains("refused the sign-in"), "{why}");
    assert_eq!(attempts(), 2, "the same call tells how it ended and doesn't try again");
    let Outcome::Failed(again) = session.ensure(start(), LONG) else { panic!("it failed") };
    assert_eq!((again, attempts()), (why, 3), "the one after that does");
    assert!(place.helpers().is_empty() && !place.julia_ran());
}

#[test]
fn a_start_that_takes_long_is_still_working_and_the_next_call_gets_it() {
    let place = Place::new("slow");
    let hold = place.state.join("hold");
    std::fs::write(&hold, "").unwrap();
    let session = place.session();
    let Outcome::StillWorking(step) = session.ensure(start(), Duration::from_millis(1500)) else { panic!("held") };
    assert!(!step.is_empty());
    assert!(matches!(session.ensure(start(), Duration::from_millis(300)), Outcome::StillWorking(_)));
    wait_for("Julia to start", || place.julia_ran());
    std::fs::remove_file(&hold).unwrap();
    let runtime = ready(session.ensure(start(), LONG));
    assert!(pid_alive(runtime.pid as i32));
    assert_eq!(place.helpers().len(), 1, "one connection, one start");
}

#[test]
fn a_connection_that_drops_comes_back_on_the_same_port() {
    let place = Place::new("reconnect");
    let session = place.session();
    let before = ready(session.ensure(start(), LONG));
    let helpers = place.helpers();
    assert!(!helpers.is_empty());
    for pid in &helpers {
        // SAFETY: plain syscall, on the helper this test's session started.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    wait_for("the helper to change", || place.helpers().iter().all(|p| !helpers.contains(p)) && !place.helpers().is_empty());
    let after = ready(session.ensure(start(), LONG));
    assert_eq!((after.port, after.pid, after.reattached), (before.port, before.pid, true), "the same listener and the same runtime");
    assert_eq!(session.port(), before.port);
    assert!(pid_alive(before.pid as i32));
}

#[test]
fn closing_leaves_the_runtime_running_and_ends_everything_else() {
    let place = Place::new("close");
    let session = place.session();
    let runtime = ready(session.ensure(start(), LONG));
    #[cfg(target_os = "linux")]
    assert!(has_thread(&format!("session-{}", runtime.port)), "the session's thread runs");
    session.close();
    assert!(pid_alive(runtime.pid as i32), "the runtime goes on");
    assert!(place.helpers().is_empty(), "the helper let go");
    wait_for("the port to close", || !listening(runtime.port));
    #[cfg(target_os = "linux")]
    assert!(!has_thread(&format!("session-{}", runtime.port)), "the session's thread ended");
    assert!(matches!(session.ensure(start(), LONG), Outcome::Failed(why) if why.contains("closed")));
    session.close();
    // The runtime is still there for the next.
    let second = place.session();
    assert_eq!(ready(second.ensure(Want::Attach { install: false }, LONG)).pid, runtime.pid);
}

#[test]
fn a_dropped_session_ends_its_thread_helper_and_port_and_stops_nothing() {
    let place = Place::new("drop");
    let session = place.session();
    let runtime = ready(session.ensure(start(), LONG));
    drop(session);
    assert!(pid_alive(runtime.pid as i32));
    assert!(place.helpers().is_empty());
    wait_for("the port to close", || !listening(runtime.port));
    #[cfg(target_os = "linux")]
    assert!(!has_thread(&format!("session-{}", runtime.port)));
    // One that never got a connection ends too, however far it got.
    let slow = Place::new("drop-connecting");
    let started = Instant::now();
    let connecting = slow.session_with(true, Some("sleep 30".into()));
    let port = connecting.port();
    drop(connecting);
    assert!(started.elapsed() < Duration::from_secs(10), "the connect was cancelled");
    wait_for("the port to close", || !listening(port));
    assert!(slow.helpers().is_empty());
}

#[test]
fn two_sessions_in_one_process_leave_each_other_alone() {
    let (one, two) = (Place::new("pair-one"), Place::new("pair-two"));
    let (first, second) = (one.session(), two.session());
    let (a, b) = (ready(first.ensure(start(), LONG)), ready(second.ensure(start(), LONG)));
    assert_ne!((a.port, a.pid), (b.port, b.pid));
    assert_ne!(first.port(), second.port());
    first.stop().expect("stop");
    assert!(!pid_alive(a.pid as i32), "the first runtime is gone");
    assert_eq!((first.status().state, second.status().state), (State::Connected, State::Ready));
    assert!(pid_alive(b.pid as i32) && listening(b.port));
    // The second goes on after the first is closed, and the first starts again.
    first.close();
    assert_eq!(ready(second.ensure(start(), Duration::ZERO)), b);
    drop(first);
    assert!(listening(b.port) && one.helpers().is_empty() && !two.helpers().is_empty());
    let third = one.session();
    assert_ne!(ready(third.ensure(start(), LONG)).pid, a.pid);
}
