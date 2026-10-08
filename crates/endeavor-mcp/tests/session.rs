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

use common::{FakeBridge, TOKEN, julia_pids, pid_alive, serving_julia, wait_for};
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

    /// The pid of the runtime's core, once it runs.
    fn runtime(&self) -> Option<i32> {
        let state: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(self.state.join("runtime.json")).ok()?).ok()?;
        state["pid"].as_i64().map(|p| p as i32)
    }

    /// End the helpers of the machine, as a network failure would.
    fn drop_connection(&self) {
        for pid in self.helpers() {
            // SAFETY: plain syscall, on a helper this test's session started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
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
    // A core that is still starting has no record, but names itself in `starting.lock`.
    let starting = std::fs::read_to_string(dir.join("runtime-state/starting.lock")).ok().and_then(|t| t.split_whitespace().next().and_then(|p| p.parse::<i64>().ok()));
    if let Some(pid) = recorded.and_then(|v| v["pid"].as_i64()).or(starting).filter(|&p| p > 1) {
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
    let runtime = ready(session.ensure(start(), Duration::MAX, false));
    assert_eq!(runtime.port, session.port(), "the runtime is reached through the listener");
    assert!(runtime.page_url.contains(&format!(":{}/", runtime.port)) && runtime.mcp_url.ends_with("/mcp"), "{runtime:?}");
    assert!(pid_alive(runtime.pid as i32) && !runtime.reattached && runtime.token.len() == 64);
    assert!(listening(runtime.port));
    assert_eq!(ready(session.ensure(start(), Duration::ZERO, false)), runtime, "asked again, it is the same");
    let status = session.status();
    assert_eq!((status.state, status.machine.as_str(), status.name.as_str()), (State::Ready, "lab-ready", "lab"));
    assert_eq!(status.runtime, Some(runtime));
    assert_eq!(status.hello.map(|h| h.helper_installed), Some(Some(true)));
}

#[test]
fn an_attach_with_nothing_running_says_so_and_a_start_after_it_starts() {
    let place = Place::new("attach");
    let session = place.session();
    assert_eq!(session.ensure(Want::Attach { install: false }, LONG, false), Outcome::NothingRunning);
    assert!(!place.julia_ran(), "no Julia was started");
    assert!(session.status().nothing_running);
    let runtime = ready(session.ensure(start(), LONG, false));
    assert!(!session.status().nothing_running, "a start clears it");
    // Another session attaches to what runs, and is not told that nothing does.
    drop(session);
    let second = place.session();
    let attached = ready(second.ensure(Want::Attach { install: false }, LONG, false));
    assert!(attached.reattached && attached.pid == runtime.pid, "{attached:?}");
}

#[test]
fn a_machine_without_the_helper_needs_the_agreement_and_the_agreement_installs_it() {
    let place = Place::new("needs-install");
    let session = place.session_with(false, None);
    let Outcome::NeedsInstall(needs) = session.ensure(Want::Start { job: None, install: false }, LONG, false) else { panic!("it needs the helper") };
    assert_eq!(needs.items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(), [wire::KIND_HELPER]);
    assert!(needs.needs_helper());
    assert!(!place.dir.join("root").exists() && place.helpers().is_empty(), "nothing was installed");
    // Asked again with the agreement, it installs and starts.
    ready(session.ensure(start(), LONG, false));
    assert!(place.dir.join("root").exists());
    assert_eq!(session.status().needs_install, None);
}

#[test]
fn a_failed_sign_in_is_kept_for_every_call_and_only_a_call_that_asks_tries_again() {
    let place = Place::new("failed");
    let asked = place.dir.join("asked");
    std::fs::create_dir_all(&place.dir).unwrap();
    let ask = format!("echo x >> {}; echo 'jc@lab: Permission denied (publickey).' >&2; exit 255", asked.display());
    let attempts = || std::fs::read_to_string(&asked).map_or(0, |text| text.lines().count());
    let session = place.session_with(true, Some(ask));
    wait_for("the first attempt to fail", || attempts() == 1 && session.status().state == State::Failed);
    let Outcome::Failed(why) = session.ensure(start(), LONG, false) else { panic!("it failed") };
    assert!(why.contains("refused the sign-in"), "{why}");
    for want in [start(), Want::Attach { install: false }, start()] {
        assert_eq!(session.ensure(want, LONG, false), Outcome::Failed(why.clone()), "every call is told, and none tries again");
    }
    assert_eq!(attempts(), 1);
    // A call that asks to try again does, and the answer is not in yet.
    assert!(matches!(session.ensure(start(), Duration::ZERO, true), Outcome::StillWorking(_)));
    wait_for("the second attempt to fail", || attempts() == 2 && session.status().state == State::Failed);
    assert_eq!((session.ensure(start(), LONG, false), attempts()), (Outcome::Failed(why.clone()), 2));
    let Outcome::Failed(third) = session.ensure(start(), LONG, true) else { panic!("it failed") };
    assert_eq!((third, attempts()), (why, 3));
    assert!(place.helpers().is_empty() && !place.julia_ran());
}

#[test]
fn a_start_that_takes_long_is_still_working_and_the_next_call_gets_it() {
    let place = Place::new("slow");
    let hold = place.state.join("hold");
    std::fs::write(&hold, "").unwrap();
    let session = place.session();
    let Outcome::StillWorking(step) = session.ensure(start(), Duration::from_millis(1500), false) else { panic!("held") };
    assert!(!step.is_empty());
    assert!(matches!(session.ensure(start(), Duration::from_millis(300), false), Outcome::StillWorking(_)));
    wait_for("Julia to start", || place.julia_ran());
    std::fs::remove_file(&hold).unwrap();
    let runtime = ready(session.ensure(start(), LONG, false));
    assert!(pid_alive(runtime.pid as i32));
    assert_eq!(place.helpers().len(), 1, "one connection, one start");
}

#[test]
fn a_connection_that_drops_comes_back_on_the_same_port() {
    let place = Place::new("reconnect");
    let session = place.session();
    let before = ready(session.ensure(start(), LONG, false));
    let helpers = place.helpers();
    assert!(!helpers.is_empty());
    for pid in &helpers {
        // SAFETY: plain syscall, on the helper this test's session started.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    wait_for("the helper to change", || place.helpers().iter().all(|p| !helpers.contains(p)) && !place.helpers().is_empty());
    let after = ready(session.ensure(start(), LONG, false));
    assert_eq!((after.port, after.pid, after.reattached), (before.port, before.pid, true), "the same listener and the same runtime");
    assert_eq!(session.port(), before.port);
    assert!(pid_alive(before.pid as i32));
}

#[test]
fn closing_leaves_the_runtime_running_and_ends_everything_else() {
    let place = Place::new("close");
    let session = place.session();
    let runtime = ready(session.ensure(start(), LONG, false));
    #[cfg(target_os = "linux")]
    assert!(has_thread(&format!("session-{}", runtime.port)), "the session's thread runs");
    session.close();
    assert!(pid_alive(runtime.pid as i32), "the runtime goes on");
    assert!(place.helpers().is_empty(), "the helper let go");
    wait_for("the port to close", || !listening(runtime.port));
    #[cfg(target_os = "linux")]
    assert!(!has_thread(&format!("session-{}", runtime.port)), "the session's thread ended");
    assert!(matches!(session.ensure(start(), LONG, false), Outcome::Failed(why) if why.contains("closed")));
    session.close();
    // The runtime is still there for the next.
    let second = place.session();
    assert_eq!(ready(second.ensure(Want::Attach { install: false }, LONG, false)).pid, runtime.pid);
}

#[test]
fn a_dropped_session_ends_its_thread_helper_and_port_and_stops_nothing() {
    let place = Place::new("drop");
    let session = place.session();
    let runtime = ready(session.ensure(start(), LONG, false));
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
    let (a, b) = (ready(first.ensure(start(), LONG, false)), ready(second.ensure(start(), LONG, false)));
    assert_ne!((a.port, a.pid), (b.port, b.pid));
    assert_ne!(first.port(), second.port());
    first.stop().expect("stop");
    assert!(!pid_alive(a.pid as i32), "the first runtime is gone");
    assert_eq!((first.status().state, second.status().state), (State::Connected, State::Ready));
    assert!(pid_alive(b.pid as i32) && listening(b.port));
    // The second goes on after the first is closed, and the first starts again.
    first.close();
    assert_eq!(ready(second.ensure(start(), Duration::ZERO, false)), b);
    drop(first);
    assert!(listening(b.port) && one.helpers().is_empty() && !two.helpers().is_empty());
    let third = one.session();
    assert_ne!(ready(third.ensure(start(), LONG, false)).pid, a.pid);
}

#[test]
fn an_agreement_goes_to_a_start_that_waits_for_it_and_to_one_that_was_refused() {
    let place = Place::new("agreement");
    // The helper is missing and the agreement comes with a later call, while the first still connects.
    let session = place.session_with(false, Some("sleep 1".into()));
    let plain = Want::Start { job: None, install: false };
    assert!(matches!(session.ensure(plain.clone(), Duration::ZERO, false), Outcome::StillWorking(_)));
    ready(session.ensure(start(), LONG, false));
    assert_eq!(session.status().hello.and_then(|h| h.helper_installed), Some(true));
    drop(session);

    // Refused and told; the agreement given on its own, then the same call goes on.
    let place = Place::new("agreement-apart");
    let session = place.session_with(false, None);
    assert!(matches!(session.ensure(plain.clone(), LONG, false), Outcome::NeedsInstall(needs) if needs.needs_helper()));
    session.allow_install();
    ready(session.ensure(plain, LONG, false));
    assert!(place.dir.join("root").exists());
}

#[test]
fn an_attach_never_replaces_a_start_and_a_start_is_never_told_nothing_runs() {
    let place = Place::new("attach-while-start");
    let hold = place.state.join("hold");
    std::fs::write(&hold, "").unwrap();
    let session = place.session();
    let (starter, attacher) = std::thread::scope(|scope| {
        let starter = scope.spawn(|| session.ensure(start(), LONG, false));
        wait_for("the start to begin", || place.julia_ran() || session.status().state == State::Starting);
        let attacher = scope.spawn(|| session.ensure(Want::Attach { install: false }, LONG, false));
        std::thread::sleep(Duration::from_millis(500));
        wait_for("Julia to start", || place.julia_ran());
        std::fs::remove_file(&hold).unwrap();
        (starter.join().unwrap(), attacher.join().unwrap())
    });
    let (started, attached) = (ready(starter), ready(attacher));
    assert_eq!(started, attached, "the attach waited for the start");

    // Asked at the same moment, the start is the one that decides, whichever comes first.
    let place = Place::new("attach-and-start");
    let session = place.session();
    for _ in 0..3 {
        let (starter, attacher) = std::thread::scope(|scope| {
            let attacher = scope.spawn(|| session.ensure(Want::Attach { install: false }, LONG, false));
            let starter = scope.spawn(|| session.ensure(start(), LONG, false));
            (starter.join().unwrap(), attacher.join().unwrap())
        });
        let runtime = ready(starter);
        assert!(matches!(attacher, Outcome::NothingRunning | Outcome::Ready(_)));
        session.stop().expect("stop");
        assert!(!pid_alive(runtime.pid as i32));
    }
}

#[test]
fn closing_during_a_start_lets_it_finish_without_the_client() {
    let place = Place::new("close-starting");
    let hold = place.state.join("hold");
    std::fs::write(&hold, "").unwrap();
    let session = place.session();
    assert!(matches!(session.ensure(start(), Duration::from_millis(1500), false), Outcome::StillWorking(_)));
    wait_for("Julia to start", || place.julia_ran());
    let began = Instant::now();
    session.close();
    assert!(began.elapsed() < Duration::from_secs(10));
    assert!(place.helpers().is_empty(), "the helper let go");
    assert!(matches!(session.ensure(start(), LONG, false), Outcome::Failed(why) if why.contains("closed")));
    std::fs::remove_file(&hold).unwrap();
    let recorded = place.state.join("runtime.json");
    wait_for("the runtime to record itself", || recorded.exists());
    let second = place.session();
    assert!(ready(second.ensure(Want::Attach { install: false }, LONG, false)).reattached, "the start finished by itself");
}

#[test]
fn closing_during_a_connect_cancels_it_and_starts_nothing() {
    let place = Place::new("close-connecting");
    let session = place.session_with(true, Some("sleep 30".into()));
    assert!(matches!(session.ensure(start(), Duration::from_millis(300), false), Outcome::StillWorking(_)));
    let began = Instant::now();
    session.close();
    assert!(began.elapsed() < Duration::from_secs(10), "the connect was cancelled");
    assert!(matches!(session.ensure(Want::Attach { install: false }, Duration::ZERO, false), Outcome::Failed(why) if why.contains("closed")));
    assert!(place.helpers().is_empty() && !place.julia_ran());
}

#[test]
fn a_stop_ends_the_runtime_and_a_start_after_it_works_on_the_same_port() {
    let place = Place::new("stop-start");
    let session = place.session();
    let first = ready(session.ensure(start(), LONG, false));
    session.stop().expect("stop");
    assert!(!pid_alive(first.pid as i32), "the runtime is gone");
    let status = session.status();
    assert_eq!((status.state, status.runtime), (State::Connected, None));
    assert!(!place.helpers().is_empty(), "the connection stays");
    let second = ready(session.ensure(start(), LONG, false));
    assert_eq!(second.port, first.port, "the same listener");
    assert!(!second.reattached && second.pid != first.pid);
    session.stop().expect("stop");
    assert!(session.stop().is_ok(), "nothing runs, and the helper says so");
}

#[test]
fn a_stop_during_a_start_is_no_failure() {
    let place = Place::new("stop-starting");
    let hold = place.state.join("hold");
    std::fs::write(&hold, "").unwrap();
    let session = place.session();
    assert!(matches!(session.ensure(start(), Duration::from_millis(1500), false), Outcome::StillWorking(_)));
    wait_for("Julia to be asked for", || place.julia_ran());
    session.stop().expect("stop");
    std::fs::remove_file(&hold).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let status = session.status();
    assert_eq!((status.state, status.error, status.runtime), (State::Connected, None, None));
}

#[test]
fn a_runtime_that_ended_while_the_connection_was_lost_is_told_to_every_call_and_not_started_again() {
    // Connecting waits while `gate` exists, so that the runtime ends while the connection is away.
    let place = Place::new("ended-away");
    let gate = place.dir.join("gate");
    let session = place.session_with(true, Some(format!("while [ -e {} ]; do sleep 0.1; done", gate.display())));
    let before = ready(session.ensure(start(), LONG, false));
    std::fs::write(&gate, "").unwrap();
    place.drop_connection();
    wait_for("the connection lost", || session.status().state == State::Connecting);
    // SAFETY: plain syscall, on the runtime this test's session started.
    unsafe { libc::kill(-(before.pid as i32), libc::SIGKILL) };
    wait_for("the runtime to end", || !pid_alive(before.pid as i32));
    std::fs::remove_file(&gate).unwrap();
    wait_for("the end to be told", || session.status().state == State::Failed);
    let Outcome::Failed(why) = session.ensure(start(), LONG, false) else { panic!("it ended") };
    assert!(why.contains("Julia on lab"), "{why}");
    assert_eq!(session.ensure(Want::Attach { install: false }, LONG, false), Outcome::Failed(why), "every call is told");
    assert!(!pid_alive(before.pid as i32) && place.runtime().is_none_or(|pid| pid == before.pid as i32), "nothing started a new one");
    // Asked again, it starts a new one.
    let mut next = None;
    wait_for("a new runtime", || {
        next = Some(session.ensure(start(), Duration::from_millis(200), true));
        matches!(next, Some(Outcome::Ready(_)))
    });
    assert_ne!(ready(next.unwrap()).pid, before.pid);
}

#[test]
fn a_connection_lost_while_the_runtime_starts_is_resumed_after_the_reconnect() {
    // Connecting waits while `gate` exists, so that the runtime is up before the connection is back.
    let place = Place::new("lost-starting");
    let (hold, gate) = (place.state.join("hold"), place.dir.join("gate"));
    std::fs::write(&hold, "").unwrap();
    let session = place.session_with(true, Some(format!("while [ -e {} ]; do sleep 0.1; done", gate.display())));
    assert!(matches!(session.ensure(start(), Duration::from_millis(1500), false), Outcome::StillWorking(_)));
    wait_for("Julia to be asked for", || place.julia_ran());
    std::fs::write(&gate, "").unwrap();
    place.drop_connection();
    wait_for("the connection lost", || session.status().state == State::Connecting);
    std::fs::remove_file(&hold).unwrap();
    wait_for("the runtime to come up without the client", || place.runtime().is_some());
    std::fs::remove_file(&gate).unwrap();
    let runtime = ready(session.ensure(start(), LONG, false));
    assert_eq!((Some(runtime.pid as i32), runtime.reattached), (place.runtime(), true));
}

#[test]
fn a_connection_lost_and_back_while_the_core_still_starts_sees_the_start_and_waits_for_it() {
    // The core holds `starting.lock` and has no record from before the connection drops until the reconnect
    // is over: `hold` keeps Julia from being ready, and `gate` keeps the connection from coming back.
    let place = Place::new("lost-starting-held");
    let (hold, gate) = (place.state.join("hold"), place.dir.join("gate"));
    std::fs::write(&hold, "").unwrap();
    let session = place.session_with(true, Some(format!("while [ -e {} ]; do sleep 0.1; done", gate.display())));
    assert!(matches!(session.ensure(start(), Duration::from_millis(1500), false), Outcome::StillWorking(_)));
    wait_for("Julia to be asked for", || place.julia_ran());
    std::fs::write(&gate, "").unwrap();
    place.drop_connection();
    wait_for("the connection lost", || session.status().state == State::Connecting);
    assert!(place.runtime().is_none(), "the core has not recorded itself");
    std::fs::remove_file(&gate).unwrap();

    wait_for("the new connection to see the start", || {
        let status = session.status();
        status.state == State::Starting && status.step.as_deref().is_some_and(|step| step.contains("waiting for it"))
    });
    std::thread::sleep(Duration::from_millis(500));
    let status = session.status();
    assert_eq!((status.state, status.error), (State::Starting, None), "not \"not running\", while the core is still starting");
    assert!(place.runtime().is_none());

    std::fs::remove_file(&hold).unwrap();
    let runtime = ready(session.ensure(start(), LONG, false));
    assert_eq!((Some(runtime.pid as i32), runtime.reattached), (place.runtime(), true), "the start it did not begin");
    assert_eq!(julia_pids(&place.state).len(), 1, "one core ran: the second connection started none");
    assert_eq!(session.status().state, State::Ready);
}

#[test]
fn asking_again_while_connecting_makes_no_other_attempt() {
    let place = Place::new("asked-again");
    let asked = place.dir.join("asked");
    let attempts = || std::fs::read_to_string(&asked).map_or(0, |text| text.lines().count());
    // The sign-in takes 2 s and then fails.
    let session = place.session_with(true, Some(format!("echo x >> {}; sleep 2; exit 1", asked.display())));
    for _ in 0..5 {
        let _ = session.ensure(start(), Duration::ZERO, false);
    }
    wait_for("the failure", || session.status().state == State::Failed);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(attempts(), 1);
}

#[test]
fn a_reconnect_installs_the_helper_again_when_it_was_agreed_to_and_asks_when_it_was_not() {
    let place = Place::new("reinstall");
    let root = place.dir.join("root");
    let session = place.session();
    let before = ready(session.ensure(start(), LONG, false));
    // The helper goes from the machine and the connection with it: the reconnect installs it again, with no new question.
    let lost = place.helpers();
    std::fs::remove_dir_all(&root).unwrap();
    place.drop_connection();
    wait_for("the helper to change", || place.helpers().iter().all(|p| !lost.contains(p)) && !place.helpers().is_empty());
    assert!(root.join(endeavor_mcp::embedded::BUILD_VERSION).join("endeavor").exists(), "installed again by the reconnect");
    assert_eq!(ready(session.ensure(start(), LONG, false)).pid, before.pid);

    // An agreement given to a start with the helper there is for what the start needs, and is not kept for the helper.
    let place = Place::new("agreement-not-kept");
    let root = place.dir.join("root");
    common::install_helper(&root);
    let session = place.session_with(false, None);
    wait_for("the connection", || session.status().state == State::Connected);
    ready(session.ensure(start(), LONG, false));
    std::fs::remove_dir_all(&root).unwrap();
    place.drop_connection();
    wait_for("the question", || session.status().state == State::NeedsInstall);
    assert!(session.status().needs_install.is_some_and(|needs| needs.needs_helper()));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!root.exists(), "nothing was installed without a question");
    session.allow_install();
    ready(session.ensure(Want::Attach { install: false }, LONG, false));
}

#[test]
fn a_connection_that_cannot_come_back_is_failed_and_kept_and_the_runtime_is_found_when_asked_to_try_again() {
    let place = Place::new("refused-for-good");
    let gone = place.dir.join("refuse");
    let ask = format!("[ ! -e {} ] || {{ echo 'jc@lab: Permission denied (publickey).' >&2; exit 255; }}", gone.display());
    let session = place.session_with(true, Some(ask));
    let before = ready(session.ensure(start(), LONG, false));
    std::fs::write(&gone, "").unwrap();
    place.drop_connection();
    wait_for("the failure", || session.status().state == State::Failed);
    let Outcome::Failed(why) = session.ensure(Want::Attach { install: false }, LONG, false) else { panic!("it failed") };
    assert!(why.contains("refused the sign-in"), "{why}");
    assert_eq!(session.ensure(start(), LONG, false), Outcome::Failed(why), "kept for the next call");
    assert!(pid_alive(before.pid as i32), "the runtime goes on");
    assert!(session.status().runtime.is_none(), "and is not claimed through a connection that is gone");
    std::fs::remove_file(&gone).unwrap();
    let after = ready(session.ensure(Want::Attach { install: false }, LONG, true));
    assert_eq!((after.pid, after.port), (before.pid, before.port), "the same runtime on the same port");
}

#[test]
fn a_runtime_that_ended_is_not_running_and_a_start_after_it_starts_another() {
    let place = Place::new("ended-here");
    let session = place.session();
    let before = ready(session.ensure(start(), LONG, false));
    // SAFETY: plain syscall, on the runtime this test's session started.
    unsafe { libc::kill(-(before.pid as i32), libc::SIGKILL) };
    wait_for("the end to be noticed", || session.status().state == State::Connected);
    assert_eq!(session.ensure(Want::Attach { install: false }, LONG, false), Outcome::NothingRunning, "it looks again, and finds none");
    assert_ne!(ready(session.ensure(start(), LONG, false)).pid, before.pid);
}

#[test]
fn an_attach_that_found_nothing_looks_again_and_finds_what_has_come_up_since() {
    let place = Place::new("looks-again");
    let session = place.session();
    assert_eq!(session.ensure(Want::Attach { install: false }, LONG, false), Outcome::NothingRunning);
    let other = place.session();
    let started = ready(other.ensure(start(), LONG, false));
    assert_eq!(ready(session.ensure(Want::Attach { install: false }, LONG, false)).pid, started.pid);
}
