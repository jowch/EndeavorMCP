//! The helper binary against a stand-in runtime (a `sleep` process plus a
//! small server on its one loopback port), or the core it starts over a
//! stand-in Julia, so no Julia is needed: file requests before any runtime,
//! attaching on request, relaying, several clients on one runtime, a runtime
//! from before one port per runtime, and each way a connection ends.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::helper::Helper;
use wire::files::{Reply, Request, RuntimeState};
use wire::{ToApp, ToHelper};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A runtime as the helper sees one: a live pid and one port, whose calls
/// check the token and exit the "runtime" on `endeavor/shutdown`, and whose
/// other paths stand for Pluto's WebSocket: switched, then echoed.
struct FakeRuntime {
    process: Arc<Mutex<Child>>,
    pid: u32,
}

impl FakeRuntime {
    fn start(dir: &Path, node: &str) -> FakeRuntime {
        FakeRuntime::start_as(dir, node, serde_json::json!({ "launcher": "process" }), Duration::ZERO)
    }

    /// One that takes `delay` to exit after it's asked to shut down.
    fn slow_to_exit(dir: &Path, node: &str, delay: Duration) -> FakeRuntime {
        FakeRuntime::start_as(dir, node, serde_json::json!({ "launcher": "process" }), delay)
    }

    /// One a Slurm job started: `node-start` wrote its state, with the job's id.
    fn in_job(dir: &Path, node: &str, job: &str) -> FakeRuntime {
        FakeRuntime::start_as(dir, node, serde_json::json!({ "launcher": "slurm", "job": job }), Duration::ZERO)
    }

    fn start_as(dir: &Path, node: &str, mut state: serde_json::Value, delay: Duration) -> FakeRuntime {
        let process = Command::new("sleep").arg("600").process_group(0).spawn().unwrap();
        let pid = process.id();
        let process = Arc::new(Mutex::new(process));
        let p = process.clone();
        let port = serve(move |socket| one_port(socket, &p, delay));
        let fields = serde_json::json!({ "node": node, "pid": pid, "port": port, "token": TOKEN });
        state.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
        FakeRuntime { process, pid }
    }

    fn alive(&self) -> bool {
        self.process.lock().unwrap().try_wait().unwrap().is_none()
    }

    fn kill(&self) {
        let mut process = self.process.lock().unwrap();
        let _ = process.kill();
        let _ = process.wait();
    }
}

impl Drop for FakeRuntime {
    fn drop(&mut self) {
        self.kill();
    }
}

fn serve(handle: impl Fn(TcpStream) + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = Arc::new(handle);
    std::thread::spawn(move || {
        for socket in listener.incoming().map_while(Result::ok) {
            let handle = handle.clone();
            std::thread::spawn(move || handle(socket));
        }
    });
    port
}

fn one_port(mut socket: TcpStream, process: &Mutex<Child>, delay: Duration) {
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let (mut request, mut auth, mut length) = (String::new(), String::new(), 0);
    reader.read_line(&mut request).unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(": ").unwrap();
        match name.to_ascii_lowercase().as_str() {
            "authorization" => auth = value.to_owned(),
            "content-length" => length = value.parse().unwrap(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    if !request.starts_with("POST /endeavor/call ") {
        let _ = socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
        let _ = std::io::copy(&mut reader, &mut socket);
        return;
    }
    if auth != format!("Bearer {TOKEN}") {
        let _ = socket.write_all(b"HTTP/1.1 401 Unauthorized\r\n\r\n");
        return;
    }
    if String::from_utf8_lossy(&body).contains("endeavor/shutdown") {
        std::thread::sleep(delay);
        let mut process = process.lock().unwrap();
        let _ = process.kill();
        let _ = process.wait();
    }
    if String::from_utf8_lossy(&body).contains("list_notebooks") {
        let _ = write!(socket, "HTTP/1.1 200 OK\r\n\r\n{}", r#"{"result":{"content":[{"text":"[{\"path\":\"a.jl\"},{\"path\":\"b.jl\"}]"}]}}"#);
        return;
    }
    let _ = write!(socket, "HTTP/1.1 200 OK\r\n\r\n{{\"said\":{:?}}}", request.trim_end());
}

impl Helper {
    fn start(dir: &Path, flags: &[&str]) -> Helper {
        let flags = [&["--julia", "/nonexistent/julia"], flags].concat();
        Helper::start_with(dir, &flags, &[])
    }

    fn start_with(dir: &Path, flags: &[&str], env: &[(&str, &str)]) -> Helper {
        let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
        command
            .args(["connect", "--state-dir"])
            .arg(dir)
            .args(["--runtime", "/nonexistent", "--depot", "/nonexistent"])
            .args(flags)
            .envs(env.iter().copied());
        Helper::spawn(command)
    }
}

/// A relayed connection to Pluto's WebSocket, switched and ready to echo.
fn websocket(helper: &Helper) -> TcpStream {
    let mut socket = helper.connect();
    write!(socket, "GET /channels HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 101 "), "{}", String::from_utf8_lossy(&head));
    socket
}

fn state_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("connect-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A call relayed through `helper` to the runtime's port.
fn call(helper: &Helper) -> String {
    let mut call = helper.connect();
    write!(call, "POST /endeavor/call HTTP/1.0\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}").unwrap();
    let mut reply = String::new();
    call.read_to_string(&mut reply).unwrap();
    reply
}

const CALL_REPLY_ENDS: &str = "{\"said\":\"POST /endeavor/call HTTP/1.0\"}";

#[test]
fn several_helpers_attach_and_one_stop_ends_the_runtime_for_all() {
    let dir = state_dir("attach");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut first = Helper::start(&dir, &["--any-node"]);
    let ToApp::Ready { launcher, node, pid, token, reattached, .. } = first.start_runtime() else { unreachable!() };
    assert_eq!((launcher.as_str(), node.as_str(), pid), ("process", "labbox3", runtime.pid));
    assert_eq!((token.as_str(), reattached), (TOKEN, true));

    // Both Pluto's WebSocket and the app's calls go to the runtime's one port.
    let mut pluto = websocket(&first);
    pluto.write_all(b"over the relay").unwrap();
    let mut back = [0; 14];
    pluto.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"over the relay");
    assert!(call(&first).starts_with("HTTP/1.1 200") && call(&first).ends_with(CALL_REPLY_ENDS));

    // A second client attaches to the same runtime; the first goes on, unasked.
    let mut second = Helper::start(&dir, &["--any-node", "--quit-with-client"]);
    let ToApp::Ready { pid: second_pid, reattached, .. } = second.start_runtime() else { unreachable!() };
    assert_eq!((second_pid, reattached), (runtime.pid, true));
    std::thread::sleep(Duration::from_millis(300));
    assert!(first.control.try_recv().is_err(), "a second client disturbed the first");
    assert!(first.process.try_wait().unwrap().is_none());
    assert!(call(&first).ends_with(CALL_REPLY_ENDS) && call(&second).ends_with(CALL_REPLY_ENDS));
    pluto.write_all(b"still here").unwrap();
    let mut back = [0; 10];
    pluto.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"still here");

    // The first leaving leaves the runtime to the second.
    first.send(ToHelper::Detach);
    first.exits();
    let mut rest = Vec::new();
    assert_eq!(pluto.read_to_end(&mut rest).unwrap_or(0), 0);
    assert!(runtime.alive() && second.control.try_recv().is_err());
    assert!(call(&second).ends_with(CALL_REPLY_ENDS));

    // Stop from one connection ends the runtime for the other, which says why.
    let mut third = Helper::start(&dir, &["--any-node"]);
    assert!(matches!(third.start_runtime(), ToApp::Ready { reattached: true, .. }));
    let stop = second.request_stop();
    assert_eq!(second.next(), ToApp::Stopped { id: stop });
    assert!(!runtime.alive());
    assert!(!dir.join("runtime.json").exists());
    assert!(second.process.try_wait().unwrap().is_none(), "a stop keeps the helper");
    let ToApp::Died { status, .. } = third.next() else { panic!("expected Died") };
    assert_eq!(status, "It was stopped from another connection.");
    assert!(third.control.try_recv().is_err(), "nothing else was sent");
    assert!(second.control.try_recv().is_err(), "the one that stopped it isn't told it died");
    for helper in [&mut second, &mut third] {
        helper.stdin.0.lock().unwrap().take();
        helper.exits();
    }
}

#[test]
fn ready_says_the_build_and_interface_the_runtime_recorded_and_none_for_a_record_without_them() {
    let dir = state_dir("ready-build");
    let runtime = FakeRuntime::start_as(&dir, "labbox3", serde_json::json!({ "launcher": "process", "build": "0abc", "interface": 7 }), Duration::ZERO);
    let mut helper = Helper::start(&dir, &["--any-node"]);
    let ToApp::Ready { build, interface, .. } = helper.start_runtime() else { panic!("expected Ready") };
    assert_eq!((build.as_deref(), interface), (Some("0abc"), Some(7)));
    helper.stdin.0.lock().unwrap().take();
    let _ = helper.process.wait();
    runtime.kill();

    let dir = state_dir("ready-no-build");
    let _runtime = FakeRuntime::start(&dir, "labbox3");
    let helper = Helper::start(&dir, &["--any-node"]);
    let ToApp::Ready { build, interface, .. } = helper.start_runtime() else { panic!("expected Ready") };
    assert_eq!((build, interface), (None, None));
}

#[test]
fn a_helper_keeps_running_and_relaying_when_it_gets_sigusr1() {
    let dir = state_dir("sigusr1");
    let _runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start(&dir, &["--any-node"]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    // SAFETY: plain syscall on our own child.
    unsafe { libc::kill(helper.process.id() as i32, libc::SIGUSR1) };
    std::thread::sleep(Duration::from_millis(300));
    assert!(helper.process.try_wait().unwrap().is_none(), "SIGUSR1 ended the helper");
    assert!(helper.control.try_recv().is_err());
    assert!(call(&helper).ends_with(CALL_REPLY_ENDS));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
    assert!(!dir.join("lock").exists(), "a helper takes no lock to hand over");
}

#[test]
fn without_a_state_dir_the_helper_uses_the_folder_serve_and_mcp_use() {
    let dir = state_dir("default-folder");
    let (home, xdg) = (dir.join("home"), dir.join("xdg"));
    let folder = xdg.join("endeavor/serve").join(this_host());
    std::fs::create_dir_all(&folder).unwrap();
    let runtime = FakeRuntime::start(&folder, &this_host());
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.args(["connect", "--julia", "/nonexistent/julia", "--runtime", "/nonexistent", "--depot", "/nonexistent"]).env("HOME", &home).env("XDG_STATE_HOME", &xdg);
    let mut helper = Helper::spawn(command);
    let ToApp::Ready { pid, reattached, .. } = helper.start_runtime() else { panic!("expected Ready") };
    assert_eq!((pid, reattached), (runtime.pid, true));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

/// The next message that isn't about how the start is going.
fn after_start(helper: &Helper) -> ToApp {
    loop {
        match helper.next() {
            ToApp::Progress { .. } | ToApp::Found { .. } | ToApp::Submitted { .. } | ToApp::Queued { .. } => {}
            other => return other,
        }
    }
}

/// The processes whose command line has `needle`.
fn running(needle: &str) -> usize {
    let out = Command::new("ps").args(["-eo", "args="]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains(needle)).count()
}

#[test]
fn helpers_asked_to_start_at_once_start_one_runtime_that_serve_then_finds() {
    let dir = state_dir("start-once");
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let mut first = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    let mut second = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    first.hello();
    second.hello();
    first.request_start(None, true);
    second.request_start(None, true);
    let (ToApp::Ready { pid: a, reattached: a_again, .. }, ToApp::Ready { pid: b, reattached: b_again, .. }) = (after_start(&first), after_start(&second)) else { panic!("expected Ready") };
    assert_eq!(a, b, "one runtime");
    assert_eq!(running(&format!("core --state-dir {}", dir.display())), 1);
    assert!(a_again != b_again, "one started it and the other attached: {a_again} {b_again}");

    // `serve` in the same folder finds it, and hears when a helper stops it.
    let mut serve = serve_in(&dir);
    let stop = first.request_stop();
    assert_eq!(first.next(), ToApp::Stopped { id: stop });
    let ToApp::Died { status, .. } = second.next() else { panic!("expected Died") };
    assert_eq!(status, "It was stopped from another connection.");
    let said = ended(serve);
    assert_eq!(said.0, Some(0));
    assert!(said.1.contains(&format!("already running from {}", dir.display())), "{}", said.1);
    assert!(said.1.ends_with("Endeavor was stopped from another connection.\n"), "{}", said.1);
    for helper in [&mut first, &mut second] {
        helper.stdin.0.lock().unwrap().take();
        helper.exits();
    }

    // The next runtime clears the note the stop left, and `endeavor stop` tells both what it was.
    assert!(std::fs::read_to_string(dir.join("stopped")).unwrap().ends_with(" connection"));
    let mut again = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    again.hello();
    again.request_start(None, true);
    assert!(matches!(after_start(&again), ToApp::Ready { reattached: false, .. }));
    assert!(!dir.join("stopped").exists());
    serve = serve_in(&dir);
    let stop = Command::new(env!("CARGO_BIN_EXE_endeavor")).args(["stop", "--state-dir"]).arg(&dir).output().unwrap();
    assert!(stop.status.success());
    let ToApp::Died { status, .. } = again.next() else { panic!("expected Died") };
    assert_eq!(status, "It was stopped with `endeavor stop`.");
    let said = ended(serve);
    assert_eq!(said.0, Some(0));
    assert!(said.1.ends_with("Endeavor was stopped with `endeavor stop`.\n"), "{}", said.1);
    again.stdin.0.lock().unwrap().take();
    again.exits();
}

/// The cores started for `dir`, ended by pid when the test is over.
struct Cores(PathBuf);

impl Cores {
    fn pids(&self) -> Vec<i32> {
        let found = Command::new("pgrep").arg("-f").arg("--").arg(format!("core --state-dir {}", self.0.display())).output().unwrap();
        String::from_utf8_lossy(&found.stdout).split_whitespace().filter_map(|p| p.parse().ok()).collect()
    }
}

impl Drop for Cores {
    fn drop(&mut self) {
        for pid in self.pids() {
            // SAFETY: plain syscalls, on a core this test started (its own process group) and what it started.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

/// A helper that has asked for a runtime whose Julia is held back, and heard the first line of its log.
fn held_start(dir: &Path, julia: &Path, flags: &[&str]) -> Helper {
    let home = dir.join("home").display().to_string();
    let env = [("HOME", home.as_str()), ("XDG_STATE_HOME", home.as_str()), ("XDG_CONFIG_HOME", home.as_str()), ("XDG_CACHE_HOME", home.as_str())];
    let helper = Helper::start_with(dir, &[&["--julia", julia.to_str().unwrap()], flags].concat(), &env);
    helper.hello();
    helper.request_start(None, true);
    loop {
        if matches!(helper.next(), ToApp::Progress { line } if line == "booting") {
            return helper;
        }
    }
}

/// `leave` ends the client of a start that is held back; the core goes on, and a helper that asks while it is still
/// starting waits for it and attaches to it.
fn a_start_outlives_its_client(name: &str, leave: impl FnOnce(&mut Helper)) {
    let dir = state_dir(name);
    let cores = Cores(dir.clone());
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    std::fs::write(dir.join("hold"), "").unwrap();
    let mut first = held_start(&dir, &julia, &[]);
    let started = cores.pids();
    assert_eq!(started.len(), 1, "{started:?}");
    leave(&mut first);
    first.exits();
    std::thread::sleep(Duration::from_millis(300));
    assert!(common::pid_alive(started[0]) && !dir.join("runtime.json").exists(), "the start goes on without its client");

    let mut second = held_start(&dir, &julia, &[]);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(cores.pids(), started, "no second runtime is started beside the one starting");
    std::fs::remove_file(dir.join("hold")).unwrap();
    let ToApp::Ready { pid, reattached, .. } = after_start(&second) else { panic!("expected Ready") };
    assert_eq!((pid as i32, reattached), (started[0], true));
    assert_eq!(cores.pids(), started);
    assert_eq!(common::read_json(&dir.join("runtime.json"))["pid"].as_i64(), Some(started[0] as i64), "it recorded itself");
    let stop = second.request_stop();
    assert_eq!(second.next(), ToApp::Stopped { id: stop });
    second.stdin.0.lock().unwrap().take();
    second.exits();
}

#[test]
fn a_start_goes_on_when_the_client_detaches() {
    a_start_outlives_its_client("start-detach", |helper| helper.send(ToHelper::Detach));
}

#[test]
fn a_start_goes_on_when_the_clients_input_ends() {
    a_start_outlives_its_client("start-eof", |helper| drop(helper.stdin.0.lock().unwrap().take()));
}

#[test]
fn a_stop_ends_a_start_and_so_does_the_end_of_input_where_the_runtime_goes_with_its_client() {
    let dir = state_dir("start-cancelled");
    let cores = Cores(dir.clone());
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    std::fs::write(dir.join("hold"), "").unwrap();

    let helper = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    let stop = helper.request_stop();
    assert_eq!(helper.after_progress(), ToApp::StartCancelled { id: 1 });
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    common::wait_for("the core to end", || !common::pid_alive(core));
    assert!(!dir.join("runtime.json").exists());

    let mut helper = held_start(&dir, &julia, &["--quit-with-client"]);
    let core = cores.pids()[0];
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
    common::wait_for("the core to end", || !common::pid_alive(core));
}

/// What the app's Test connection asks for: a start of its own goes with the client, whether still starting or
/// ready, and one it waited for or found running is left.
#[test]
fn the_end_of_input_stops_only_a_start_of_its_own_with_own_with_client() {
    let (dir, cores, julia) = held_dir("start-own-eof");
    let mut own = held_start(&dir, &julia, &["--own-with-client"]);
    let core = cores.pids()[0];
    own.stdin.0.lock().unwrap().take();
    own.exits();
    common::wait_for("the core to end", || !common::pid_alive(core));
    assert!(!dir.join("runtime.json").exists());

    let first = held_start(&dir, &julia, &[]);
    let started = cores.pids();
    assert_eq!(started.len(), 1, "{started:?}");
    let mut waiter = held_start(&dir, &julia, &["--own-with-client"]);
    waiter.stdin.0.lock().unwrap().take();
    waiter.exits();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(cores.pids(), started, "a start another connection began goes on");
    std::fs::remove_file(dir.join("hold")).unwrap();
    assert!(matches!(after_start(&first), ToApp::Ready { reattached: false, .. }));

    let mut found = Helper::start(&dir, &["--own-with-client"]);
    assert!(matches!(found.start_runtime(), ToApp::Ready { reattached: true, .. }));
    found.stdin.0.lock().unwrap().take();
    found.exits();
    std::thread::sleep(Duration::from_millis(300));
    assert!(common::pid_alive(started[0]) && dir.join("runtime.json").exists(), "a runtime it found running is left");
    let stop = first.request_stop();
    assert_eq!(first.next(), ToApp::Stopped { id: stop });
    common::wait_for("the core to end", || !common::pid_alive(started[0]));

    std::fs::write(dir.join("hold"), "").unwrap();
    let mut own = held_start(&dir, &julia, &["--own-with-client"]);
    let core = cores.pids()[0];
    std::fs::remove_file(dir.join("hold")).unwrap();
    assert!(matches!(after_start(&own), ToApp::Ready { reattached: false, .. }));
    own.stdin.0.lock().unwrap().take();
    own.exits();
    common::wait_for("the core to end", || !common::pid_alive(core));
    assert!(!dir.join("runtime.json").exists());
    first.stdin.0.lock().unwrap().take();
}

/// `dir` with a fake Julia that is held back, and the guard that ends the cores started in it.
fn held_dir(name: &str) -> (PathBuf, Cores, PathBuf) {
    let dir = state_dir(name);
    let cores = Cores(dir.clone());
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    std::fs::write(dir.join("hold"), "").unwrap();
    (dir, cores, julia)
}

/// A binary of ours run in `dir`'s own folders.
fn endeavor(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.args(args).arg("--state-dir").arg(dir);
    for var in ["HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"] {
        command.env(var, dir.join("home"));
    }
    command
}

#[test]
fn a_stop_during_a_start_whose_client_has_gone_stops_nothing_and_says_it_is_still_starting() {
    let (dir, cores, julia) = held_dir("start-stop-cli");
    let mut helper = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    helper.send(ToHelper::Detach);
    helper.exits();

    // The lock the stop needs is held for a moment only, and the stop waits for it a while.
    let held = hold_start_lock(&dir);
    let refused = endeavor(&dir, &["stop"]).env("ENDEAVOR_STOP_LOCK_SECS", "1").output().unwrap();
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("Julia was not stopped"), "{refused:?}");
    drop(held);

    let refused = endeavor(&dir, &["stop"]).output().unwrap();
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success() && said.contains("still starting") && said.contains("endeavor stop --force"), "{refused:?}");
    let mut other = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    other.hello();
    let stop = other.request_stop();
    let ToApp::NotStopped { id, message } = other.next() else { panic!("expected NotStopped") };
    assert_eq!(id, stop);
    assert!(message.contains("still starting"), "{message}");
    assert!(common::pid_alive(core) && cores.pids() == [core], "nothing was stopped");

    // It comes up, and is then found and stopped as usual.
    std::fs::remove_file(dir.join("hold")).unwrap();
    other.request_start(None, true);
    let ToApp::Ready { pid, reattached, .. } = after_start(&other) else { panic!("expected Ready") };
    assert_eq!((pid as i32, reattached), (core, true));
    let stopped = endeavor(&dir, &["stop"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&stopped.stdout), format!("Stopped Julia (pid {core}).\n"));
    common::wait_for("the core to end", || !common::pid_alive(core));
    other.stdin.0.lock().unwrap().take();
    other.exits();
}

/// Whether nothing holds `starting.lock` in `dir`.
fn start_lock_free(dir: &Path) -> bool {
    std::fs::File::open(dir.join("starting.lock")).is_ok_and(|file| file.try_lock_shared().is_ok())
}

#[test]
fn a_forced_stop_cancels_a_start_that_is_under_way_and_leaves_nothing_of_it() {
    let (dir, cores, julia) = held_dir("start-force-cli");
    let mut first = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    let julia_pid = common::julia_pids(&dir)[0];
    first.send(ToHelper::Detach);
    first.exits();
    // Another client waits for the start the first began.
    let waiter = held_start(&dir, &julia, &[]);
    assert_eq!(cores.pids(), [core]);

    // The helper's check, which a client asks after a reconnect, sees the start.
    let mut asker = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    asker.hello();
    assert_eq!(check(&asker, 70), RuntimeState::Starting);

    let status = endeavor(&dir, &["status", "--json"]).output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!((status["runtime"]["starting"].clone(), status["runtime"]["starting_pid"].clone()), (serde_json::json!(true), serde_json::json!(core)), "{status}");
    let text = String::from_utf8_lossy(&endeavor(&dir, &["status"]).output().unwrap().stdout).into_owned();
    assert!(text.contains(&format!("Start under way: yes (pid {core}")), "{text}");

    let held = hold_start_lock(&dir);
    let cancel = endeavor(&dir, &["stop", "--force"]).env("ENDEAVOR_STOP_LOCK_SECS", "1").output().unwrap();
    assert!(!cancel.status.success() && common::pid_alive(core), "the start lock is held: {cancel:?}");
    drop(held);
    let cancel = endeavor(&dir, &["stop", "--force"]).output().unwrap();
    assert!(cancel.status.success(), "{cancel:?}");
    assert_eq!(String::from_utf8_lossy(&cancel.stdout), format!("Cancelled the start of Julia (pid {core}).\n"));

    common::wait_for("the core and Julia to end", || !common::pid_alive(core) && !common::pid_alive(julia_pid));
    common::wait_for("the lock to be let go", || start_lock_free(&dir));
    assert!(cores.pids().is_empty(), "{:?}", cores.pids());
    assert!(!dir.join("runtime.json").exists() && !dir.join("runtime.json.tmp").exists(), "no record was left");
    assert_eq!(std::fs::read_to_string(dir.join("stopped")).unwrap(), format!("{core} stop"), "the clients are told why");
    let ToApp::StartFailed { message, .. } = waiter.after_progress() else { panic!("expected StartFailed") };
    assert!(message.contains("stopped while it was starting") && message.contains("`endeavor stop`"), "the waiter is told, and does not start another: {message}");
    assert!(cores.pids().is_empty() && common::julia_pids(&dir) == [julia_pid]);
    assert_eq!(check(&asker, 71), RuntimeState::NotRunning, "and a cancelled start is none");
    asker.stdin.0.lock().unwrap().take();
    asker.exits();

    // Nothing is in the way of the next start.
    std::fs::remove_file(dir.join("hold")).unwrap();
    let mut next = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    next.hello();
    next.request_start(None, true);
    let ToApp::Ready { pid, reattached, .. } = after_start(&next) else { panic!("expected Ready") };
    assert!(!reattached && pid as i32 != core);
    let again = endeavor(&dir, &["stop", "--force"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&again.stdout), format!("Stopped Julia (pid {pid}).\n"), "a runtime that is up is stopped as usual");
    let none = endeavor(&dir, &["stop", "--force"]).output().unwrap();
    assert!(none.status.success() && String::from_utf8_lossy(&none.stdout).starts_with("No Julia is running"), "{none:?}");
    next.stdin.0.lock().unwrap().take();
    next.exits();
}

#[test]
fn a_forced_stop_from_a_helper_cancels_a_start_another_connection_began() {
    let (dir, cores, julia) = held_dir("start-force-helper");
    let mut first = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    first.send(ToHelper::Detach);
    first.exits();

    // A connection that waits for that start: an unforced stop ends its wait, leaves the start alone and says so.
    let waiter = held_start(&dir, &julia, &[]);
    let stop = waiter.request_stop();
    assert_eq!(waiter.after_progress(), ToApp::StartCancelled { id: 1 });
    let ToApp::NotStopped { id, message } = waiter.after_progress() else { panic!("expected NotStopped") };
    assert!(id == stop && message.contains("still starting"), "{message}");
    assert!(common::pid_alive(core), "the start goes on");

    // A forced one cancels it, and is answered once it is gone.
    waiter.request_start(None, true);
    let stop = waiter.request_stop_as(true);
    let ToApp::StartCancelled { .. } = waiter.after_progress() else { panic!("expected StartCancelled") };
    assert_eq!(waiter.after_progress(), ToApp::Stopped { id: stop });
    common::wait_for("the core to end", || !common::pid_alive(core));
    common::wait_for("the lock to be let go", || start_lock_free(&dir));
    assert!(cores.pids().is_empty() && !dir.join("runtime.json").exists(), "{:?}", cores.pids());
    assert_eq!(std::fs::read_to_string(dir.join("stopped")).unwrap(), format!("{core} connection"), "the clients are told why");
    drop(waiter);

    // A connection that isn't waiting: unforced, nothing is stopped; forced, the start is cancelled.
    let mut first = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    first.send(ToHelper::Detach);
    first.exits();
    let mut other = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    other.hello();
    let stop = other.request_stop();
    let ToApp::NotStopped { id, message } = other.next() else { panic!("expected NotStopped") };
    assert!(id == stop && message.contains("still starting") && message.contains("force"), "{message}");
    assert!(common::pid_alive(core));
    let stop = other.request_stop_as(true);
    assert_eq!(other.next(), ToApp::Stopped { id: stop });
    common::wait_for("the core to end", || !common::pid_alive(core));
    common::wait_for("the lock to be let go", || start_lock_free(&dir));
    assert!(cores.pids().is_empty() && !dir.join("runtime.json").exists(), "{:?}", cores.pids());
    let stop = other.request_stop_as(true);
    assert_eq!(other.next(), ToApp::Stopped { id: stop }, "nothing runs or starts, and a forced stop says so");
    other.stdin.0.lock().unwrap().take();
    other.exits();
}

#[test]
fn an_attach_that_waits_for_a_start_never_starts_a_runtime_when_that_start_dies() {
    let (dir, cores, julia) = held_dir("attach-only");
    let env_home = dir.join("home").display().to_string();
    let attaching = || {
        let env = [("HOME", env_home.as_str()), ("XDG_STATE_HOME", env_home.as_str()), ("XDG_CONFIG_HOME", env_home.as_str()), ("XDG_CACHE_HOME", env_home.as_str())];
        let helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &env);
        helper.hello();
        let id = helper.request_attach();
        (helper, id)
    };
    // Nothing runs and nothing starts: it says so, and starts nothing.
    let (none, id) = attaching();
    assert_eq!(none.after_progress(), ToApp::NotRunning { id });
    assert!(cores.pids().is_empty() && common::julia_pids(&dir).is_empty(), "nothing was started");

    // A start is under way: the attach waits for it, and when it dies says nothing runs.
    let mut first = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    first.send(ToHelper::Detach);
    first.exits();
    let (waiter, id) = attaching();
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: plain syscall, on the core this test's helper started and what it started.
    unsafe { libc::kill(-core, libc::SIGKILL) };
    assert_eq!(waiter.after_progress(), ToApp::NotRunning { id });
    std::thread::sleep(Duration::from_millis(500));
    assert!(cores.pids().is_empty(), "no core was started in its place: {:?}", cores.pids());
    assert_eq!(common::julia_pids(&dir).len(), 1, "exactly one Julia ran, the one that was waited for");

    // One that comes up is attached to.
    let mut second = held_start(&dir, &julia, &[]);
    let (attached, id) = attaching();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_file(dir.join("hold")).unwrap();
    let ToApp::Ready { pid, reattached, .. } = after_start(&attached) else { panic!("expected Ready for {id}") };
    assert!(reattached && pid as i32 == cores.pids()[0]);
    second.stdin.0.lock().unwrap().take();
    second.exits();
}

#[test]
fn the_helpers_check_says_a_start_is_under_way_beside_a_hung_runtime_and_not_after_it() {
    let dir = state_dir("check-silent");
    let runtime = FakeRuntime::start(&dir, &this_host());
    // Its port stops answering: a runtime that hangs.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut state = common::read_json(&dir.join("runtime.json"));
    state["port"] = closed.into();
    std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
    let helper = Helper::start(&dir, &[]);
    helper.hello();
    assert_eq!(check(&helper, 1), RuntimeState::NotRunning, "a hung runtime and no start under way");
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("starting.lock")).unwrap();
    lock.lock().unwrap();
    assert_eq!(check(&helper, 2), RuntimeState::Starting, "its replacement is on the way");
    drop(lock);
    assert_eq!(check(&helper, 3), RuntimeState::NotRunning);
    assert!(runtime.alive());
    helper.stdin.0.lock().unwrap().take();
}

/// A runtime's port that takes connections and says nothing for `quiet`, as a computer waking from sleep
/// or a runtime under heavy load does, and then answers every call.
fn quiet_for(quiet: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let until = std::time::Instant::now() + quiet;
    std::thread::spawn(move || {
        for mut socket in listener.incoming().flatten() {
            if std::time::Instant::now() < until {
                continue;
            }
            // The whole request first: answering and closing with part of it unread resets the connection.
            let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
            while socket.read(&mut [0; 1024]).is_ok_and(|n| n > 0) {}
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        }
    });
    port
}

/// The runtime recorded in `dir` now listens on `port`.
fn record_port(dir: &Path, port: u16) {
    let mut state = common::read_json(&dir.join("runtime.json"));
    state["port"] = port.into();
    std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
}

#[test]
fn a_runtime_that_is_silent_for_a_while_is_waited_for_and_used_and_none_is_started_beside_it() {
    let dir = state_dir("silent-a-while");
    let cores = Cores(dir.clone());
    let runtime = FakeRuntime::start(&dir, &this_host());
    record_port(&dir, quiet_for(Duration::from_secs(2)));
    let julia = fake_julia(&dir);
    let helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[("ENDEAVOR_TEST_SILENT_WAIT_SECS", "20")]);
    helper.hello();
    helper.request_start(None, true);
    let ToApp::Progress { line } = helper.next() else { panic!("expected Progress") };
    assert_eq!(line, format!("Julia here (pid {}) isn't answering; asking it again for up to 20 seconds.", runtime.pid));
    let ToApp::Ready { pid, reattached, .. } = after_start(&helper) else { panic!("expected Ready") };
    assert_eq!((pid, reattached), (runtime.pid, true));
    assert!(cores.pids().is_empty() && runtime.alive());
    helper.stdin.0.lock().unwrap().take();
}

#[test]
fn a_stop_while_a_silent_runtime_is_asked_again_stops_it_as_a_stop_with_nothing_attached_does() {
    let dir = state_dir("silent-stop");
    let runtime = FakeRuntime::start(&dir, &this_host());
    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    record_port(&dir, closed);
    let julia = fake_julia(&dir);
    let helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[("ENDEAVOR_TEST_SILENT_WAIT_SECS", "20")]);
    helper.hello();
    let start = helper.request_start(None, true);
    assert!(matches!(helper.next(), ToApp::Progress { line } if line.contains("isn't answering")));
    // The start doesn't stop what it didn't begin; the stop that ends its wait does, as the user asked.
    let stop = helper.request_stop();
    assert_eq!(helper.after_progress(), ToApp::StartCancelled { id: start });
    assert_eq!(helper.after_progress(), ToApp::Stopped { id: stop });
    assert!(!runtime.alive() && !dir.join("runtime.json").exists());
    helper.stdin.0.lock().unwrap().take();
}

#[test]
fn a_runtime_that_stays_silent_is_neither_stopped_nor_replaced_and_every_client_says_why() {
    let dir = state_dir("silent-stuck");
    let cores = Cores(dir.clone());
    let runtime = FakeRuntime::start(&dir, &this_host());
    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    record_port(&dir, closed);
    let julia = fake_julia(&dir);
    let wait = ("ENDEAVOR_TEST_SILENT_WAIT_SECS", "2");
    let said = format!("Julia here (pid {}) is running but hasn't answered for 2 seconds, so no second one was started beside it.", runtime.pid);

    let helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[wait]);
    helper.hello();
    let began = std::time::Instant::now();
    helper.request_start(None, true);
    let ToApp::StartFailed { message, .. } = after_start(&helper) else { panic!("expected StartFailed") };
    assert!(message.starts_with(&said) && message.contains("`endeavor stop`"), "{message}");
    assert!(began.elapsed() >= Duration::from_secs(2), "{:?}", began.elapsed());
    helper.stdin.0.lock().unwrap().take();

    let serve = endeavor(&dir, &["serve", "--julia", julia.to_str().unwrap(), "--depot", "/opt/depot:"]).env(wait.0, wait.1).output().unwrap();
    let stderr = String::from_utf8_lossy(&serve.stderr);
    assert!(serve.status.code() == Some(1) && stderr.contains(&format!("endeavor: {said}")), "{serve:?}");

    assert!(runtime.alive(), "not stopped");
    assert!(cores.pids().is_empty() && common::julia_pids(&dir).is_empty(), "nothing was started");
}

#[test]
fn a_forced_stop_does_not_end_a_core_the_file_does_not_name() {
    let (dir, cores, julia) = held_dir("start-force-unnamed");
    let mut first = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    first.send(ToHelper::Detach);
    first.exits();
    // The file names the core by its pid, start time and computer; a signal goes on all three, never on a guess.
    let named = common::read_json(&dir.join("starting.lock"));
    assert_eq!((named["pid"].as_i64(), named["node"].as_str()), (Some(core as i64), Some(this_host().as_str())), "the core names itself: {named}");
    for (what, change) in [("a start time that is not its own", ("started", serde_json::json!(1))), ("no start time", ("started", serde_json::Value::Null)), ("another computer", ("node", serde_json::json!("another-node")))] {
        let mut file = named.clone();
        file[change.0] = change.1;
        std::fs::write(dir.join("starting.lock"), file.to_string()).unwrap();
        let refused = endeavor(&dir, &["stop", "--force"]).output().unwrap();
        assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("can't tell which process"), "{what}: {refused:?}");
        assert!(common::pid_alive(core) && !dir.join("stopped").exists(), "{what}: nothing was signalled");
    }
}

#[test]
fn a_recorded_pid_that_started_at_another_time_is_stale_and_a_stop_leaves_the_process_alone() {
    let dir = state_dir("stale-start-time");
    let runtime = FakeRuntime::start(&dir, &this_host());
    let record = |started: serde_json::Value| {
        let mut state = common::read_json(&dir.join("runtime.json"));
        state["started"] = started;
        std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
    };
    // The pid belongs to a process that did not start then: a record from before a reboot.
    record(serde_json::json!(1));
    let status: serde_json::Value = serde_json::from_slice(&endeavor(&dir, &["status", "--json"]).output().unwrap().stdout).unwrap();
    assert_eq!((status["runtime"]["state"].clone(), status["runtime"]["answers"].clone()), (serde_json::json!("stale"), serde_json::Value::Null), "{status}");
    let stop = endeavor(&dir, &["stop", "--force"]).output().unwrap();
    assert!(stop.status.success() && String::from_utf8_lossy(&stop.stdout).starts_with("No Julia is running"), "{stop:?}");
    assert!(runtime.alive(), "the process was not signalled, and its port was not asked to shut down");
    assert!(!dir.join("runtime.json").exists() && !dir.join("stopped").exists());
}

#[test]
fn a_core_killed_while_starting_leaves_the_next_client_to_start_a_new_one_at_once() {
    let (dir, cores, julia) = held_dir("start-killed");
    let first = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    // SAFETY: plain syscall, on the core this test's helper started.
    unsafe { libc::kill(core, libc::SIGKILL) };
    assert!(matches!(first.after_progress(), ToApp::StartDied { .. }));
    std::fs::remove_file(dir.join("hold")).unwrap();

    let second = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    second.hello();
    let began = std::time::Instant::now();
    second.request_start(None, true);
    let ToApp::Ready { pid, reattached, .. } = after_start(&second) else { panic!("expected Ready") };
    assert!(pid as i32 != core && !reattached, "a new runtime, started by the second helper");
    assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
    assert_eq!(cores.pids(), [pid as i32]);
}

#[test]
fn the_julia_a_killed_core_leaves_behind_is_stopped_before_the_next_start() {
    let dir = state_dir("core-killed");
    let cores = Cores(dir.clone());
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let mut first = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    first.hello();
    first.request_start(None, true);
    let ToApp::Ready { pid: core, .. } = after_start(&first) else { panic!("expected Ready") };
    first.send(ToHelper::Detach);
    first.exits();
    let left = common::julia_pids(&dir)[0];
    // SAFETY: plain syscall, on the core alone, as an OOM kill or `kill -9` would end it.
    unsafe { libc::kill(core as i32, libc::SIGKILL) };
    common::wait_for("the core to end", || !common::pid_alive(core as i32));
    // Linux ends Julia with its core (PR_SET_PDEATHSIG); on macOS nothing does, so it is still running here.
    assert!(cfg!(target_os = "linux") || common::pid_alive(left), "Julia outlived its core");
    assert!(dir.join("runtime.json").exists(), "the killed core left its record");

    std::fs::write(dir.join("hold"), "").unwrap();
    let mut second = held_start(&dir, &julia, &[]);
    let started = common::julia_pids(&dir);
    assert_eq!(started.len(), 2, "{started:?}");
    assert!(!common::pid_alive(left), "the Julia left behind was stopped before another started");
    std::fs::remove_file(dir.join("hold")).unwrap();
    let ToApp::Ready { pid, reattached, .. } = after_start(&second) else { panic!("expected Ready") };
    assert!(pid != core && !reattached);
    assert_eq!(cores.pids(), [pid as i32]);
    let stop = second.request_stop();
    assert_eq!(second.next(), ToApp::Stopped { id: stop });
    second.stdin.0.lock().unwrap().take();
    second.exits();
}

#[test]
fn a_helper_whose_binary_was_replaced_never_starts_a_core_of_the_new_build() {
    let dir = state_dir("replaced");
    let cores = Cores(dir.clone());
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("endeavor");
    std::fs::copy(env!("CARGO_BIN_EXE_endeavor"), &exe).unwrap();
    let mut command = Command::new(&exe);
    command.args(["connect", "--state-dir"]).arg(&dir).args(["--runtime", "/nonexistent", "--depot", "/nonexistent", "--julia", julia.to_str().unwrap()]);
    let mut helper = Helper::spawn(command);
    helper.hello();
    // As `endeavor update` puts a new build in place: renamed over the running one.
    let part = bin.join("endeavor.part");
    std::fs::write(&part, "#!/bin/sh\necho 'endeavor 9.9.9 (build 9.9.9-0123456789abcdef)'\n").unwrap();
    std::fs::set_permissions(&part, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::rename(&part, &exe).unwrap();
    helper.request_start(None, true);
    if cfg!(target_os = "linux") {
        // The core starts from the file the helper runs from, which the rename did not remove.
        let ToApp::Ready { pid, .. } = after_start(&helper) else { panic!("expected Ready") };
        let program = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
        assert!(program.to_string_lossy().ends_with("(deleted)"), "{}", program.display());
        let stop = helper.request_stop();
        assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    } else {
        let ToApp::StartFailed { message, .. } = after_start(&helper) else { panic!("expected StartFailed") };
        assert!(message.contains("was replaced") && message.contains("build 9.9.9-0123456789abcdef") && message.contains("again"), "{message}");
        assert!(cores.pids().is_empty() && common::julia_pids(&dir).is_empty(), "nothing was started");
    }
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn clients_that_come_during_a_start_whose_client_has_gone_wait_for_the_one_core_and_leaving_does_not_stop_it() {
    let (dir, cores, julia) = held_dir("start-waiters");
    let mut first = held_start(&dir, &julia, &[]);
    let core = cores.pids()[0];
    first.send(ToHelper::Detach);
    first.exits();

    // A waiter that is stopped, one whose input ends under `--quit-with-client`, and `serve` interrupted: none of them started it.
    let stopped = held_start(&dir, &julia, &[]);
    let stop = stopped.request_stop();
    assert_eq!(stopped.after_progress(), ToApp::StartCancelled { id: 1 });
    // Its wait is over, and it says the start goes on rather than that it stopped.
    let ToApp::NotStopped { id, message } = stopped.after_progress() else { panic!("expected NotStopped") };
    assert!(id == stop && message.contains("still starting"), "{message}");
    let mut quitting = held_start(&dir, &julia, &["--quit-with-client"]);
    quitting.stdin.0.lock().unwrap().take();
    quitting.exits();
    let mut serve = endeavor(&dir, &["serve", "--julia", "/nonexistent/julia", "--depot", "/opt/depot:"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(700));
    // SAFETY: plain syscall, on the `serve` this test started.
    unsafe { libc::kill(serve.id() as i32, libc::SIGINT) };
    let (code, said) = ended(serve);
    assert!(code == Some(1) && said.trim_end().ends_with("endeavor: Stopped before Julia was ready."), "{code:?} {said}");
    assert!(common::pid_alive(core) && cores.pids() == [core], "the start went on");

    // Two more wait, and both get that core.
    let (mut second, mut third) = (held_start(&dir, &julia, &[]), held_start(&dir, &julia, &[]));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(cores.pids(), [core], "no other core is started beside it");
    std::fs::remove_file(dir.join("hold")).unwrap();
    for helper in [&second, &third] {
        let ToApp::Ready { pid, reattached, .. } = after_start(helper) else { panic!("expected Ready") };
        assert_eq!((pid as i32, reattached), (core, true));
    }
    assert_eq!(cores.pids(), [core]);
    let stop = second.request_stop();
    assert_eq!(second.next(), ToApp::Stopped { id: stop });
    let ToApp::Died { .. } = third.next() else { panic!("expected Died") };
    for helper in [&mut second, &mut third] {
        helper.stdin.0.lock().unwrap().take();
        helper.exits();
    }
}

/// `endeavor serve` in `dir`, once it has found the runtime there.
fn serve_in(dir: &Path) -> Child {
    let mut serve = Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .arg("serve")
        .arg("--state-dir")
        .arg(dir)
        .args(["--julia", "/nonexistent/julia", "--depot", "/opt/depot:"])
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(serve.stdout.take().unwrap());
    let mut line = String::new();
    while !line.starts_with("Ctrl-C leaves Endeavor running") {
        line.clear();
        if out.read_line(&mut line).unwrap() == 0 {
            let _ = serve.kill();
            panic!("serve ended before it found the runtime");
        }
    }
    // Kept open: `serve` would end on a broken pipe if it printed again.
    std::mem::forget(out);
    serve
}

/// How `serve` ended: its exit code and what it said on stderr.
fn ended(serve: Child) -> (Option<i32>, String) {
    let output = serve.wait_with_output().unwrap();
    (output.status.code(), String::from_utf8(output.stderr).unwrap())
}

#[test]
fn answers_file_requests_before_any_runtime() {
    let dir = state_dir("files");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("decay-fits/sub")).unwrap();
    std::fs::write(home.join("decay-fits/fit.jl"), "### A Pluto.jl notebook ###\n\n# ╔═╡ 1a2b3c4d-0000-4000-8000-000000000001\nx = 1\n").unwrap();
    let helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[("HOME", home.to_str().unwrap())]);
    let ToApp::Hello { home: said, .. } = helper.hello() else { unreachable!() };
    assert_eq!(said, home.display().to_string());
    let ask = |id: u32, request: Request| {
        helper.send(ToHelper::Files { id, request });
        match helper.next() {
            ToApp::Files { id: got, reply } if got == id => reply,
            other => panic!("expected Files {id}, got {other:?}"),
        }
    };
    let Reply::List { path, entries } = ask(1, Request::List { path: "~/decay-fits".into() }) else { panic!() };
    assert_eq!(path, home.join("decay-fits").canonicalize().unwrap());
    assert_eq!(entries.iter().map(|e| (e.name.as_str(), e.dir)).collect::<Vec<_>>(), [("sub", true), ("fit.jl", false)]);
    let Reply::Notebooks { found } = ask(2, Request::Notebooks { path: "~/decay-fits".into() }) else { panic!() };
    assert_eq!(found.len(), 1);
    let Reply::Preview { preview } = ask(3, Request::Preview { path: "~/decay-fits/fit.jl".into() }) else { panic!() };
    assert_eq!(preview.cells[0].code, "x = 1");
    assert!(matches!(ask(4, Request::List { path: "~/nope".into() }), Reply::Error { .. }));
    assert!(!dir.join("start.lock").exists(), "no runtime was asked for, so no lock");
}

#[test]
fn saves_a_sent_file_into_the_session_folder_and_drops_an_unfinished_one() {
    let dir = state_dir("upload");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("decay-fits")).unwrap();
    let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[("HOME", home.to_str().unwrap())]);
    assert!(matches!(helper.hello(), ToApp::Hello { uploads: true, .. }));
    let ask = |id: u32, request: Request| {
        helper.send(ToHelper::Files { id, request });
        match helper.next() {
            ToApp::Files { id: got, reply } if got == id => reply,
            other => panic!("expected Files {id}, got {other:?}"),
        }
    };
    let folder = "~/decay-fits".to_string();
    // SHA-256 of "t,y\n0,1\n".
    let sha256 = "b659e80980e7375313bf70ebf6e577f4abae7d5657bf7b563fa64c2f48a33eee".to_string();
    let placed = ask(1, Request::Place { folder: folder.clone(), name: "decay.csv".into(), size: 8, sha256: sha256.clone() });
    assert_eq!(placed, Reply::Place { path: "data/decay.csv".into(), have: false });
    let piece = |offset: u64, bytes: &[u8], last| Request::Write { folder: folder.clone(), path: "data/decay.csv".into(), offset, bytes: bytes.to_vec(), last };
    assert_eq!(ask(2, piece(0, b"t,y\n", false)), Reply::Written);
    assert_eq!(ask(3, piece(4, b"0,1\n", true)), Reply::Written);
    assert_eq!(std::fs::read_to_string(home.join("decay-fits/data/decay.csv")).unwrap(), "t,y\n0,1\n");
    let again = ask(4, Request::Place { folder: folder.clone(), name: "decay.csv".into(), size: 8, sha256 });
    assert_eq!(again, Reply::Place { path: "data/decay.csv".into(), have: true });
    let escape = ask(5, Request::Write { folder: folder.clone(), path: "../x.csv".into(), offset: 0, bytes: b"x".to_vec(), last: true });
    assert!(matches!(escape, Reply::Error { message } if message.contains("isn't inside")));
    assert!(!home.join("x.csv").exists());

    // The app goes mid-send: the helper deletes the part on its way out.
    assert_eq!(ask(6, piece(0, b"t,y\n", false)), Reply::Written);
    let part = home.join("decay-fits/data/.decay.csv.part");
    assert!(part.exists());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
    assert!(!part.exists(), "an unfinished part is removed when the app goes");
    assert_eq!(std::fs::read_to_string(home.join("decay-fits/data/decay.csv")).unwrap(), "t,y\n0,1\n", "the finished file stays");
}

/// Ask the helper what runs from its state folder.
fn check(helper: &Helper, id: u32) -> RuntimeState {
    helper.send(ToHelper::Files { id, request: Request::Runtime });
    match helper.next() {
        ToApp::Files { id: got, reply: Reply::Runtime { runtime } } if got == id => runtime,
        other => panic!("expected Runtime {id}, got {other:?}"),
    }
}

#[test]
fn checks_and_stops_a_runtime_without_attaching() {
    let dir = state_dir("check");
    let helper = Helper::start(&dir, &["--any-node"]);
    helper.hello();
    assert_eq!(check(&helper, 1), RuntimeState::NotRunning);
    let runtime = FakeRuntime::start(&dir, "labbox3");
    assert_eq!(check(&helper, 2), RuntimeState::Running { node: "labbox3".into(), notebooks: Some(2), job: None });
    assert!(!dir.join("start.lock").exists(), "checking takes nothing over");
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert!(!runtime.alive(), "Stop reaches a runtime nobody had attached to");
    assert_eq!(check(&helper, 3), RuntimeState::NotRunning);
}

#[test]
fn detaching_leaves_the_runtime_and_quit_with_client_stops_it_on_eof() {
    let dir = state_dir("detach");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    // The app's own word at quit wins over the flag.
    let mut helper = Helper::start(&dir, &["--any-node", "--quit-with-client"]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    helper.send(ToHelper::Detach);
    helper.exits();
    assert!(runtime.alive() && dir.join("runtime.json").exists());

    let mut helper = Helper::start(&dir, &["--any-node"]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
    assert!(runtime.alive() && dir.join("runtime.json").exists(), "without the flag, the app vanishing leaves it");

    let mut helper = Helper::start(&dir, &["--any-node", "--quit-with-client"]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
    assert!(!runtime.alive(), "the app going away stops it with --quit-with-client");
    assert!(!dir.join("runtime.json").exists());
}

#[test]
fn a_runtime_that_dies_is_reported_with_its_log() {
    let dir = state_dir("died");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    std::fs::write(dir.join("runtime.log"), "booting\nGo to http://localhost:1234/?secret=abc123 now\nERROR: boom\n").unwrap();
    let mut helper = Helper::start(&dir, &["--any-node"]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    runtime.kill();
    let ToApp::Died { status, log_tail } = helper.next() else { panic!("expected Died") };
    assert_eq!(status, "exited");
    assert_eq!(log_tail, ["booting", "Go to http://localhost:1234/?secret=… now", "ERROR: boom"]);
    assert!(!dir.join("runtime.json").exists());
    // Still connected: asking again tries to start a new one (no Julia here).
    helper.request_start(None, true);
    assert!(matches!(helper.next(), ToApp::StartFailed { .. }));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_runtime_recorded_on_another_node_is_not_replaced() {
    let dir = state_dir("node");
    let _runtime = FakeRuntime::start(&dir, "some-other-node");
    let mut helper = Helper::start(&dir, &[]);
    let ToApp::StartFailed { message, .. } = helper.start_runtime() else { panic!("expected StartFailed") };
    assert!(message.contains("some-other-node"), "{message}");
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
    assert!(dir.join("runtime.json").exists());
}

#[test]
fn a_runtime_from_before_one_port_is_not_attached_and_stop_ends_it() {
    let dir = state_dir("older");
    let older = Command::new("sleep").arg("600").process_group(0).spawn().unwrap();
    let pid = older.id();
    let ended = std::thread::spawn(move || {
        let mut older = older;
        older.wait().unwrap()
    });
    let state = serde_json::json!({
        "launcher": "process", "node": "labbox3", "pid": pid, "pluto_port": 1, "mcp_port": 2,
        "token": TOKEN, "pluto_secret": "s3cret", "job": "", "mcp": "http",
    });
    std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
    let mut helper = Helper::start(&dir, &["--any-node"]);
    let ToApp::StartFailed { message, .. } = helper.start_runtime() else { panic!("expected StartFailed") };
    assert_eq!(message, "Julia here was started by an older version of Endeavor, which this version can't connect to. Restart Julia to use it.");
    assert_eq!(check(&helper, 1), RuntimeState::Running { node: "labbox3".into(), notebooks: None, job: None }, "it still shows as running");
    assert!(!ended.is_finished(), "nothing stopped it yet");
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert!(ended.join().unwrap().signal().is_some(), "Stop ended it");
    assert!(!dir.join("runtime.json").exists());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_runtime_that_cant_start_is_an_error() {
    let dir = state_dir("nojulia");
    let mut helper = Helper::start(&dir, &[]);
    let ToApp::StartFailed { message, .. } = helper.start_runtime() else { panic!("expected StartFailed") };
    assert!(message.contains("/nonexistent/julia"), "{message}");
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

/// A julia that says it's 1.12 and then fails to boot.
fn fake_julia(dir: &Path) -> PathBuf {
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let julia = bin.join("julia");
    common::write_executable(&julia, "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'julia version 1.12.0'; exit 0; }\necho 'ERROR: boom'\nexit 3\n");
    julia
}

#[test]
fn julia_from_a_shell_line_is_found_and_its_failure_reported() {
    let dir = state_dir("shell-julia");
    let julia = fake_julia(&dir);
    let line = format!("PATH={}:$PATH", julia.parent().unwrap().display());
    let mut helper = Helper::start_with(&dir, &["--julia-shell", &line], &[("SHELL", "/bin/sh")]);
    assert_eq!(helper.start_runtime(), ToApp::Found { name: "Julia".into(), path: julia.display().to_string(), version: "1.12.0".into() });
    let ToApp::StartDied { id, status, log_tail } = helper.after_progress() else { panic!("expected StartDied") };
    assert_eq!(id, 1, "it ends the start that asked for it");
    assert!(status.contains('3'), "{status}");
    assert_eq!(log_tail, ["ERROR: boom"]);
    helper.stdin.0.lock().unwrap().take();
    helper.exits();

    // A login shell's profile can put a julia back on PATH (GitHub's Ubuntu image has one), so the line empties it.
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let line = format!("PATH={}", empty.display());
    let mut helper = Helper::start_with(&dir, &["--julia-shell", &line], &[("SHELL", "/bin/sh")]);
    let ToApp::StartFailed { message, .. } = helper.start_runtime() else { panic!("expected StartFailed") };
    assert!(message.contains(&format!("`{line}`")) && message.contains("PATH"), "{message}");
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn starts_the_core_which_starts_julia_and_stop_ends_both() {
    let dir = state_dir("core");
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let mut helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    assert!(matches!(helper.start_runtime(), ToApp::Found { .. }));
    let ToApp::Ready { pid, token, reattached, .. } = helper.after_progress() else { panic!("expected Ready") };
    assert_eq!((token.as_str(), reattached), (TOKEN, false));
    let core = pid as i32;
    let julia = common::read_json(&dir.join("julia.json"))["pid"].as_i64().unwrap() as i32;
    let ps = |field: &str, pid: i32| {
        let out = Command::new("ps").args(["-o", &format!("{field}="), "-p", &pid.to_string()]).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    assert!(ps("command", core).contains("endeavor core"), "{}", ps("command", core));
    assert_eq!(ps("ppid", julia), core.to_string(), "Julia is the core's child");
    assert_eq!((ps("pgid", julia), ps("pgid", core)), (core.to_string(), core.to_string()), "one process group, the core's");

    // Both go to the core: the folder on to Julia's bridge, the WebSocket to Pluto.
    let mut call = helper.connect();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"endeavor/set_folder","params":{"path":"/n"}}"#;
    write!(call, "POST /endeavor/call HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut reply = String::new();
    call.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200") && reply.contains(r#""result":{}"#), "{reply}");
    let folder_given = |seen: &common::Seen| seen.line.starts_with("POST /call") && String::from_utf8_lossy(&seen.body).contains(r#""path":"/n""#);
    common::wait_for("Julia's bridge to get the folder", || bridge.seen().iter().any(folder_given));
    let mut pluto = websocket(&helper);
    pluto.write_all(b"to Pluto").unwrap();
    let mut back = [0; 8];
    pluto.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"to Pluto");

    // Stop reaches Julia through the core, and neither is left.
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert!(!common::pid_alive(core) && !common::pid_alive(julia));
    assert!(!dir.join("runtime.json").exists() && !dir.join("julia.json").exists());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_runtime_started_with_exit_idle_ends_once_no_notebook_is_open() {
    let idle = [("ENDEAVOR_IDLE_CHECK_SECS", "0.2"), ("ENDEAVOR_IDLE_HOURS", "0.0003")];

    // Without the flag the runtime stays up well past the idle limit.
    let dir = state_dir("exit-idle-off");
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let mut helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &idle);
    assert!(matches!(helper.start_runtime(), ToApp::Found { .. }));
    let ToApp::Ready { pid, .. } = helper.after_progress() else { panic!("expected Ready") };
    std::thread::sleep(Duration::from_secs(3));
    assert!(common::pid_alive(pid as i32), "the idle stop of 48 hours is the runtime's only end");
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    helper.stdin.0.lock().unwrap().take();
    helper.exits();

    // With it, the runtime the helper starts ends by itself.
    let dir = state_dir("exit-idle-on");
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let mut helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap(), "--exit-idle"], &idle);
    assert!(matches!(helper.start_runtime(), ToApp::Found { .. }));
    let ToApp::Ready { pid, .. } = helper.after_progress() else { panic!("expected Ready") };
    let core = pid as i32;
    common::wait_for("the runtime to end itself", || !common::pid_alive(core));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_cluster_job_is_submitted_with_exit_idle_and_its_node_start_passes_it_on() {
    let dir = state_dir("slurm-exit-idle");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut command = slurm.command(&julia, 100);
    command.arg("--state-dir").arg(&dir).arg("--exit-idle");
    let helper = Helper::spawn(command);
    helper.hello();
    helper.request_start(small_job(), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    let script = slurm.read("job.sh");
    assert!(script.trim_end().ends_with("--build '1.0.0-abc' --exit-idle"), "{script}");

    // What the job runs: the core it becomes ends when idle.
    let node = state_dir("slurm-exit-idle-node");
    let bridge = common::FakeBridge::start(&node);
    let serving = common::serving_julia(&node, &bridge);
    std::fs::write(node.join("token"), TOKEN).unwrap();
    let mut core = Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .args(["node-start", "--state-dir"])
        .arg(&node)
        .arg("--julia")
        .arg(&serving)
        .args(["--runtime", "/nonexistent", "--depot", "/nonexistent", "--exit-idle"])
        .env("ENDEAVOR_IDLE_CHECK_SECS", "0.2")
        .env("ENDEAVOR_IDLE_HOURS", "0.0003")
        .spawn()
        .unwrap();
    common::wait_for("the job's runtime to end itself", || core.try_wait().unwrap().is_some());
}

/// Slurm's commands as scripts over files in `dir/slurm`: the test moves a job
/// through the queue by writing its state (and node, time left, `sacct`'s answer).
struct FakeSlurm {
    dir: PathBuf,
    bin: PathBuf,
}

impl FakeSlurm {
    fn new(dir: &Path) -> FakeSlurm {
        let bin = dir.join("slurmbin");
        let state = dir.join("slurm");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let scripts = [
            ("sbatch", "echo \"$@\" >> \"$FAKE_SLURM/sbatch.args\"\nfor a; do last=$a; done\ncp \"$last\" \"$FAKE_SLURM/job.sh\"\nn=$(cat \"$FAKE_SLURM/next\" 2>/dev/null || echo 42)\nif [ \"$n\" = 42 ]; then echo PENDING > \"$FAKE_SLURM/state\"; else echo PENDING > \"$FAKE_SLURM/state.$n\"; fi\necho Priority > \"$FAKE_SLURM/reason\"\necho $n\n"),
            (
                "squeue",
                "for a; do [ \"$p\" = -j ] && job=$a; p=$a; done\nstate=$(cat \"$FAKE_SLURM/state.$job\" 2>/dev/null || cat \"$FAKE_SLURM/state\" 2>/dev/null)\ncase \"$*\" in *\"-t all\"*) echo \"$state\"; exit 0;; esac\ncase \"$state\" in PENDING|RUNNING) ;; *) exit 0;; esac\necho \"$state|$(cat \"$FAKE_SLURM/reason\")|$(cat \"$FAKE_SLURM/node\" 2>/dev/null)|$(cat \"$FAKE_SLURM/left\" 2>/dev/null || echo 8:00:00)\"\n",
            ),
            ("scancel", "echo \"$@\" >> \"$FAKE_SLURM/scancel.log\"\nif [ -f \"$FAKE_SLURM/state.$1\" ]; then echo CANCELLED > \"$FAKE_SLURM/state.$1\"; else echo CANCELLED > \"$FAKE_SLURM/state\"; fi\n"),
            ("sacct", "cat \"$FAKE_SLURM/sacct\" 2>/dev/null\nexit 0\n"),
            (
                "srun",
                // Like srun, it holds the step's output until the step ends unless --unbuffered.
                "echo \"$@\" >> \"$FAKE_SLURM/srun.args\"\ncase \" $* \" in *\" --unbuffered \"*) u=1;; esac\nwhile [ $# -gt 0 ]; do case \"$1\" in -*) shift;; *) break;; esac; done\n[ -n \"$u\" ] && exec \"$@\"\n\"$@\" > \"$FAKE_SLURM/srun.out\"\ncat \"$FAKE_SLURM/srun.out\"\n",
            ),
            ("sinfo", "echo 'shared*|8:00:00|10|7492'\n"),
        ];
        for (name, body) in scripts {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        FakeSlurm { dir: state, bin }
    }

    fn set(&self, file: &str, value: &str) {
        std::fs::write(self.dir.join(file), format!("{value}\n")).unwrap();
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.dir.join(file)).unwrap_or_default()
    }

    /// A helper on this "login node", submitting with `julia`.
    fn helper(&self, state_dir: &Path, julia: &Path) -> Helper {
        self.helper_polling(state_dir, julia, 100)
    }

    /// `helper`, asking `squeue` about a waiting job every `poll_ms`.
    fn helper_polling(&self, state_dir: &Path, julia: &Path, poll_ms: u32) -> Helper {
        let mut command = self.command(julia, poll_ms);
        command.arg("--state-dir").arg(state_dir);
        Helper::spawn(command)
    }

    /// `endeavor connect` for this "login node", without a state folder.
    fn command(&self, julia: &Path, poll_ms: u32) -> Command {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
        command
            .args(["connect", "--runtime", "/nonexistent", "--depot", "/nonexistent"])
            .args(["--launcher", "slurm", "--julia", julia.to_str().unwrap(), "--build", "1.0.0-abc"])
            .env("PATH", path)
            .env("FAKE_SLURM", &self.dir)
            .env("ENDEAVOR_SLURM_POLL_MS", poll_ms.to_string())
            .env("SCRATCH", "/scratch/jc");
        command
    }
}

#[test]
fn an_r_shell_line_reaches_the_job_script_word_for_word() {
    let dir = state_dir("slurm-r-shell");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let line = r#"module load R && export R_CHECK="a b $HOME" X='it''s' `echo tick`"#;
    let mut command = slurm.command(&julia, 100);
    command.arg("--state-dir").arg(&dir).args(["--r-shell", line]);
    let helper = Helper::spawn(command);
    helper.hello();
    helper.request_start(small_job(), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    let script = slurm.read("job.sh");
    assert!(Command::new("sh").arg("-n").arg(dir.join("job.sh")).status().unwrap().success(), "{script}");
    // The script's words, as sh reads them, with printf in the helper's place.
    let exec = script.lines().find_map(|l| l.strip_prefix("exec ")).unwrap();
    let words = Command::new("sh").arg("-c").arg(format!("set -- {exec}; shift; printf '%s\\n' \"$@\"")).output().unwrap();
    let words: Vec<String> = String::from_utf8(words.stdout).unwrap().lines().map(str::to_owned).collect();
    let at = words.iter().position(|w| w == "--r-shell").expect("--r-shell is in the job script");
    assert_eq!(words[at + 1], line, "{script}");
    drop(helper);
}

#[test]
fn a_start_that_may_not_install_submits_no_job_and_lists_what_it_needs() {
    let dir = state_dir("slurm-no-julia");
    let slurm = FakeSlurm::new(&dir);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let path = format!("{}:/usr/bin:/bin", slurm.bin.display());
    let find = Command::new("/bin/sh").args(["-lc", "command -v julia"]).env("HOME", &home).env("PATH", &path).output().unwrap();
    if find.status.success() {
        eprintln!("skipped: a login shell finds julia at {}", String::from_utf8_lossy(&find.stdout).trim());
        return;
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command
        .args(["connect", "--runtime", "/nonexistent", "--depot", "/nonexistent", "--launcher", "slurm", "--julia", "auto", "--build", "1.0.0-abc", "--state-dir"])
        .arg(&dir)
        .env("HOME", &home)
        .env("SHELL", "/bin/sh")
        .env("PATH", &path)
        .env("FAKE_SLURM", &slurm.dir)
        .env("SCRATCH", "/scratch/jc");
    let helper = Helper::spawn(command);
    helper.hello();
    let start = helper.request_start(small_job(), false);
    let ToApp::NeedsInstall { id, items } = helper.next() else { panic!("expected NeedsInstall") };
    assert_eq!(id, start);
    assert_eq!(items.len(), 1, "{items:?}");
    let julia = &items[0];
    assert_eq!(julia.kind, wire::KIND_RUNTIME);
    assert!(julia.name.starts_with("Julia ") && julia.size_mb.is_some_and(|mb| mb > 100), "{julia:?}");
    assert_eq!(julia.place.as_deref(), Some(home.join(".cache/endeavor").join(format!("julia-{}", &julia.name["Julia ".len()..])).to_str().unwrap()));
    assert_eq!(slurm.read("sbatch.args"), "", "no job was submitted");
    assert!(!home.join(".cache").exists(), "nothing was downloaded");
}

#[test]
fn a_start_for_an_engine_nobody_knows_fails_and_the_helper_stays_up() {
    let dir = state_dir("unknown-engine");
    let helper = Helper::start(&dir, &[]);
    helper.hello();
    helper.send(ToHelper::StartRuntime { id: 7, job: None, engine: "marimo".into(), install: true, attach_only: false });
    let ToApp::StartFailed { id, message } = helper.next() else { panic!("expected StartFailed") };
    assert_eq!(id, 7);
    assert!(message.contains("marimo"), "{message}");
    helper.request_stop();
    assert!(matches!(helper.next(), ToApp::Stopped { .. } | ToApp::NotStopped { .. }), "it still answers");
}

fn this_host() -> String {
    String::from_utf8(Command::new("hostname").output().unwrap().stdout).unwrap().trim().to_owned()
}

fn small_job() -> Option<wire::slurm::JobRequest> {
    let resources = wire::slurm::Resources { partition: Some("short".into()), cpus: 2, mem_gb: 8, minutes: 30, ..Default::default() };
    Some(wire::slurm::JobRequest { resources, account: Some("lab".into()), depot: None })
}

#[test]
fn a_cluster_job_is_submitted_waits_runs_relays_and_ends() {
    let dir = state_dir("slurm");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut helper = slurm.helper(&dir, &julia);
    let ToApp::Hello { slurm: slurm_here, .. } = helper.hello() else { unreachable!() };
    assert!(slurm_here, "sinfo is on the PATH");
    helper.request_start(small_job(), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert_eq!(helper.next(), ToApp::Submitted { job: "42".into(), summary: "2 CPUs · 8 GB · 30 min".into() });
    assert_eq!(helper.next(), ToApp::Queued { job: "42".into(), state: "PENDING".into(), reason: "Priority".into() });
    let sbatch = slurm.read("sbatch.args");
    assert!(sbatch.contains("--parsable --job-name=endeavor"), "{sbatch}");
    assert!(sbatch.contains("--account=lab --partition=short --cpus-per-task=2 --mem=8G --time=30"), "{sbatch}");
    assert!(sbatch.contains(&format!("--output={}", dir.join("runtime.log").display())), "{sbatch}");
    let script = slurm.read("job.sh");
    assert!(script.contains("node-start") && script.contains("--depot '/scratch/jc/endeavor/depot:' --build '1.0.0-abc'"), "{script}");
    assert!(script.contains(" --r 'auto' --runtime "), "R is the node's login shell's: {script}");
    assert!(dir.join("job.json").exists());

    // Still queued, for another reason; then it runs and its runtime comes up.
    slurm.set("reason", "Resources");
    assert_eq!(helper.next(), ToApp::Queued { job: "42".into(), state: "PENDING".into(), reason: "Resources".into() });
    slurm.set("node", &this_host());
    slurm.set("left", "29:30");
    slurm.set("state", "RUNNING");
    assert_eq!(helper.next(), ToApp::Queued { job: "42".into(), state: "RUNNING".into(), reason: this_host() });
    let runtime = FakeRuntime::in_job(&dir, &this_host(), "42");
    let ToApp::Ready { launcher, job: Some(job), reattached, .. } = helper.after_progress() else { panic!("expected Ready") };
    assert_eq!((launcher.as_str(), job.id.as_str(), job.route.as_str(), reattached), ("slurm", "42", "srun", false));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(job.ends_at.unwrap().abs_diff(now + 1770) < 5, "ends in 29:30");
    assert!(slurm.read("srun.args").contains("--jobid=42 --overlap"));
    assert!(!dir.join("job.json").exists(), "a running job's record is runtime.json");

    // Streams go through both helpers.
    let mut pluto = websocket(&helper);
    pluto.write_all(b"via the node").unwrap();
    let mut back = [0; 12];
    pluto.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"via the node");

    // A new connect (from any login node) finds the running job.
    helper.send(ToHelper::Detach);
    helper.exits();
    let mut rest = Vec::new();
    assert_eq!(pluto.read_to_end(&mut rest).unwrap_or(0), 0);
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    helper.request_start(None, true);
    let ToApp::Ready { reattached, job: Some(job), .. } = helper.after_progress() else { panic!("expected Ready") };
    assert!(reattached && job.id == "42");
    assert_eq!(slurm.read("sbatch.args").lines().count(), 1, "no second job");
    let mut pluto = websocket(&helper);
    pluto.write_all(b"again").unwrap();
    let mut back = [0; 5];
    pluto.read_exact(&mut back).unwrap();

    // The job hits its time limit: Slurm kills Julia, and the app hears why.
    slurm.set("state", "TIMEOUT");
    slurm.set("sacct", "TIMEOUT");
    runtime.kill();
    let ToApp::Died { status, .. } = helper.after_progress() else { panic!("expected Died") };
    assert_eq!(status, "Its Slurm job reached its time limit.");
    let mut rest = Vec::new();
    assert_eq!(pluto.read_to_end(&mut rest).unwrap_or(0), 0, "its streams close");
    assert!(!dir.join("runtime.json").exists());
}

#[test]
fn a_queued_job_survives_a_disconnect_and_stop_cancels_it() {
    let dir = state_dir("slurm-queued");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut helper = slurm.helper(&dir, &julia);
    helper.hello();
    helper.request_start(small_job(), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    helper.send(ToHelper::Detach);
    helper.exits();
    assert_eq!(slurm.read("scancel.log"), "", "leaving doesn't cancel");

    // The next connect waits on the same job instead of submitting another.
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    let start = helper.request_start(None, true);
    assert_eq!(helper.next(), ToApp::Submitted { job: "42".into(), summary: "2 CPUs · 8 GB · 30 min".into() });
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    assert_eq!(slurm.read("sbatch.args").lines().count(), 1);
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::StartCancelled { id: start });
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert_eq!(slurm.read("scancel.log").trim(), "42");
    assert!(!dir.join("job.json").exists());
}

#[test]
fn a_job_that_ends_before_julia_is_ready_says_why() {
    let dir = state_dir("slurm-early");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    helper.request_start(small_job(), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    std::fs::write(dir.join("runtime.log"), "ERROR: out of disk quota\n").unwrap();
    slurm.set("sacct", "FAILED");
    slurm.set("state", "FAILED");
    let ToApp::StartFailed { message, .. } = helper.after_progress() else { panic!("expected StartFailed") };
    assert_eq!(message, "Its Slurm job failed. Julia wasn't ready yet. Its last output: ERROR: out of disk quota");
}

#[test]
fn stopping_a_running_job_shuts_julia_down_then_cancels_the_job() {
    let dir = state_dir("slurm-stop");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    helper.request_start(small_job(), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    slurm.set("node", &this_host());
    slurm.set("state", "RUNNING");
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    let runtime = FakeRuntime::in_job(&dir, &this_host(), "42");
    assert!(matches!(helper.after_progress(), ToApp::Ready { .. }));

    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert!(!runtime.alive(), "the runtime was asked to shut down");
    assert_eq!(slurm.read("scancel.log").trim(), "42");
    assert!(!dir.join("runtime.json").exists());
}

#[test]
fn a_cluster_check_sees_the_job_and_stop_cancels_it_without_attaching() {
    let dir = state_dir("slurm-check");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut first = slurm.helper(&dir, &julia);
    first.hello();
    first.request_start(small_job(), true);
    assert!(matches!(first.after_progress(), ToApp::Found { .. }));
    assert!(matches!(first.next(), ToApp::Submitted { .. }));
    assert!(matches!(first.next(), ToApp::Queued { .. }));
    first.send(ToHelper::Detach);
    first.exits();

    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    assert_eq!(check(&helper, 1), RuntimeState::Queued { job: "42".into(), state: "PENDING".into(), reason: "Priority".into() });
    slurm.set("node", &this_host());
    slurm.set("state", "RUNNING");
    assert_eq!(check(&helper, 2), RuntimeState::Queued { job: "42".into(), state: "RUNNING".into(), reason: this_host() });
    let _runtime = FakeRuntime::in_job(&dir, &this_host(), "42");
    let RuntimeState::Running { node, notebooks: None, job: Some(job) } = check(&helper, 3) else { panic!("expected Running") };
    assert_eq!((node, job.id.as_str(), job.node), (this_host(), "42", this_host()));
    assert!(job.ends_at.is_some());
    assert_eq!(slurm.read("srun.args"), "", "checking doesn't reach the node");

    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert_eq!(slurm.read("scancel.log").trim(), "42");
    assert!(!dir.join("runtime.json").exists() && !dir.join("job.json").exists());
    assert_eq!(check(&helper, 4), RuntimeState::NotRunning);
}

#[test]
fn two_helpers_on_a_cluster_share_one_job_and_a_stop_from_one_ends_it_for_both() {
    let dir = state_dir("slurm-two");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut first = slurm.helper(&dir, &julia);
    let mut second = slurm.helper(&dir, &julia);
    first.hello();
    second.hello();
    // Both ask at once: one submits, the other waits for that job.
    first.request_start(small_job(), true);
    second.request_start(small_job(), true);
    for helper in [&first, &second] {
        loop {
            match helper.next() {
                ToApp::Queued { job, .. } => break assert_eq!(job, "42"),
                ToApp::Progress { .. } | ToApp::Found { .. } | ToApp::Submitted { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    assert_eq!(slurm.read("sbatch.args").lines().count(), 1, "one job for two helpers");

    // A third arrives while the job is still queued, and waits on it as well.
    let third = slurm.helper(&dir, &julia);
    assert_eq!(third.start_runtime(), ToApp::Submitted { job: "42".into(), summary: "2 CPUs · 8 GB · 30 min".into() });
    assert_eq!(slurm.read("sbatch.args").lines().count(), 1);

    slurm.set("node", &this_host());
    slurm.set("state", "RUNNING");
    let _runtime = FakeRuntime::in_job(&dir, &this_host(), "42");
    for helper in [&first, &second, &third] {
        let ToApp::Ready { job: Some(job), .. } = after_start(helper) else { panic!("expected Ready") };
        assert_eq!(job.id, "42");
    }
    assert_eq!(slurm.read("sbatch.args").lines().count(), 1);

    let stop = first.request_stop();
    assert_eq!(first.next(), ToApp::Stopped { id: stop });
    for helper in [&second, &third] {
        let ToApp::Died { status, .. } = helper.next() else { panic!("expected Died") };
        assert_eq!(status, "It was stopped from another connection.");
    }
    assert!(slurm.read("scancel.log").lines().all(|l| l == "42"), "only its job is cancelled");
    for helper in [&mut first, &mut second] {
        helper.stdin.0.lock().unwrap().take();
        helper.exits();
    }
}

/// Wait until the helper says its job is queued: the job's id.
fn queued(helper: &Helper) -> String {
    loop {
        match helper.next() {
            ToApp::Queued { job, .. } => return job,
            ToApp::Progress { .. } | ToApp::Found { .. } | ToApp::Submitted { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn extra_sbatch_flags_that_could_change_what_runs_are_not_submitted() {
    let dir = state_dir("slurm-extra-flags");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut helper = slurm.helper(&dir, &julia);
    helper.hello();
    for (extra, why) in [(["--wrap=sleep 1"], "--wrap"), (["--wra=x"], "--wrap"), (["normal"], "doesn't start with"), (["--x\ny"], "line break")] {
        let mut job = small_job().unwrap();
        job.resources.extra = extra.map(String::from).to_vec();
        helper.request_start(Some(job), true);
        let ToApp::StartFailed { message, .. } = helper.after_progress() else { panic!("expected StartFailed") };
        assert!(message.starts_with("The job wasn't submitted:") && message.contains(why), "{message}");
    }
    assert_eq!(slurm.read("sbatch.args"), "", "nothing reached sbatch");
    let mut job = small_job().unwrap();
    job.resources.extra = vec!["--qos=normal".into(), "-N1".into()];
    helper.request_start(Some(job), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert_eq!(helper.next(), ToApp::Submitted { job: "42".into(), summary: "2 CPUs · 8 GB · 30 min".into() });
    assert!(slurm.read("sbatch.args").contains("--time=30 --qos=normal -N1"), "{}", slurm.read("sbatch.args"));
    helper.request_stop();
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_stop_while_the_job_is_queued_is_told_to_the_others_and_a_late_cleanup_spares_the_next_job() {
    let dir = state_dir("slurm-late");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut a = slurm.helper(&dir, &julia);
    let mut b = slurm.helper_polling(&dir, &julia, 1500);
    a.hello();
    b.hello();
    let a_start = a.request_start(small_job(), true);
    assert_eq!(queued(&a), "42");
    b.request_start(small_job(), true);
    assert_eq!(queued(&b), "42");

    // A stops the job and asks again, which submits the next one, before B looks again.
    let stop = a.request_stop();
    assert_eq!(a.next(), ToApp::StartCancelled { id: a_start });
    assert_eq!(a.next(), ToApp::Stopped { id: stop });
    slurm.set("next", "43");
    a.request_start(small_job(), true);
    assert_eq!(queued(&a), "43");
    let ToApp::StartFailed { message, .. } = b.next() else { panic!("expected StartFailed") };
    assert_eq!(message, "The start was stopped. It was stopped from another connection.");
    assert_eq!(common::read_json(&dir.join("job.json"))["job"], "43", "B's cleanup of job 42 left job 43's record");

    // A third helper waits for job 43 and doesn't submit another.
    let c = slurm.helper(&dir, &julia);
    assert_eq!(c.start_runtime(), ToApp::Submitted { job: "43".into(), summary: "2 CPUs · 8 GB · 30 min".into() });
    assert_eq!(slurm.read("sbatch.args").lines().count(), 2, "one job for 42, one for 43");
    for helper in [&mut a, &mut b] {
        helper.stdin.0.lock().unwrap().take();
        helper.exits();
    }
}

#[test]
fn a_slurm_helper_stopping_a_process_runtime_leaves_it_and_leaves_no_note() {
    let dir = state_dir("slurm-wrong-launcher");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let runtime = FakeRuntime::start(&dir, &this_host());
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert!(runtime.alive() && dir.join("runtime.json").exists());
    assert!(!dir.join("stopped").exists());
}

#[test]
fn the_default_state_folder_is_the_hosts_for_a_process_and_one_for_the_cluster_for_slurm() {
    let dir = state_dir("default-cluster");
    let (home, xdg) = (dir.join("home"), dir.join("xdg"));
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut command = slurm.command(&julia, 100);
    command.env("HOME", &home).env("XDG_STATE_HOME", &xdg);
    let helper = Helper::spawn(command);
    helper.hello();
    helper.request_start(small_job(), true);
    assert_eq!(queued(&helper), "42");
    assert!(xdg.join("endeavor/cluster/job.json").exists(), "a cluster's folder has no host name in it");
    assert!(!xdg.join("endeavor/serve").exists());

    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.args(["connect", "--julia", "/nonexistent/julia", "--runtime", "/nonexistent", "--depot", "/nonexistent"]).env("HOME", &home).env("XDG_STATE_HOME", &xdg);
    let helper = Helper::spawn(command);
    helper.hello();
    helper.request_start(None, true);
    assert!(matches!(helper.next(), ToApp::StartFailed { .. }));
    assert!(xdg.join("endeavor/serve").join(this_host()).join("start.lock").exists());
}

#[test]
fn an_attach_only_start_on_a_cluster_submits_nothing_when_no_job_runs_or_waits() {
    let dir = state_dir("slurm-attach-only");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    let id = helper.request_attach();
    assert_eq!(helper.next(), ToApp::NotRunning { id });
    assert_eq!(slurm.read("sbatch.args"), "", "no job was submitted");
    assert!(!dir.join("job.json").exists());
}

#[test]
fn launcher_auto_is_slurm_where_sinfo_is_and_says_so() {
    let dir = state_dir("launcher-auto");
    let (home, xdg) = (dir.join("home"), dir.join("xdg"));
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let mut command = slurm.command(&julia, 100);
    command.args(["--launcher", "auto"]).env("HOME", &home).env("XDG_STATE_HOME", &xdg);
    let helper = Helper::spawn(command);
    let ToApp::Hello { launcher, slurm: slurm_here, .. } = helper.hello() else { unreachable!() };
    assert_eq!((launcher.as_str(), slurm_here), ("slurm", true));
    helper.request_start(small_job(), true);
    assert_eq!(queued(&helper), "42");
    assert!(xdg.join("endeavor/cluster/job.json").exists(), "the cluster's state folder");

    // With no sinfo to be found it runs the runtime as a process (unless this computer has Slurm in a folder `has` always looks in).
    let fixed = wire::slurm::FOLDERS.iter().any(|d| Path::new(d).join("sinfo").is_file());
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.args(["connect", "--launcher", "auto", "--julia", "/nonexistent/julia", "--runtime", "/nonexistent", "--depot", "/nonexistent"]).env("PATH", "/nonexistent").env("HOME", &home).env("XDG_STATE_HOME", &xdg);
    let helper = Helper::spawn(command);
    let ToApp::Hello { launcher, .. } = helper.hello() else { unreachable!() };
    assert_eq!(launcher, if fixed { "slurm" } else { "process" });
}

/// Hold `dir/start.lock`, as a helper in the middle of a start does.
fn hold_start_lock(dir: &Path) -> std::fs::File {
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("start.lock")).unwrap();
    // SAFETY: plain syscall on a file we hold open.
    assert_eq!(unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    file
}

#[test]
fn a_helper_waiting_for_another_start_says_so_once_and_still_hears_the_client() {
    let dir = state_dir("start-wait");
    let _held = hold_start_lock(&dir);
    let line = "Another connection is starting Julia here; waiting for it.";

    // Detach is honoured while it waits.
    let mut helper = Helper::start(&dir, &[]);
    helper.hello();
    helper.request_start(None, true);
    assert_eq!(helper.next(), ToApp::Progress { line: line.into() });
    std::thread::sleep(Duration::from_millis(700));
    assert!(helper.control.try_recv().is_err(), "it says so once");
    helper.send(ToHelper::Detach);
    helper.exits();

    // So is the end of the client's input.
    let mut helper = Helper::start(&dir, &[]);
    assert_eq!(helper.start_runtime(), ToApp::Progress { line: line.into() });
    helper.stdin.0.lock().unwrap().take();
    helper.exits();

    // And Stop, which starts nothing: the start ends with an answer of its own, and the Stop with its, which
    // is a stop's as from a helper with nothing attached: it waits for the lock, and says when it gives up.
    let helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[("ENDEAVOR_STOP_LOCK_SECS", "1")]);
    helper.hello();
    let start = helper.request_start(None, true);
    assert_eq!(helper.next(), ToApp::Progress { line: line.into() });
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::StartCancelled { id: start });
    let ToApp::NotStopped { id, message } = helper.next() else { panic!("expected NotStopped") };
    assert!(id == stop && message.contains("start lock"), "{message}");
    assert!(!dir.join("runtime.json").exists());

    // A holder that never lets go is given up on.
    let helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[("ENDEAVOR_START_LOCK_SECS", "1")]);
    assert_eq!(helper.start_runtime(), ToApp::Progress { line: line.into() });
    let ToApp::StartFailed { message, .. } = helper.next() else { panic!("expected StartFailed") };
    assert!(message.contains("Gave up waiting") && message.contains(&dir.display().to_string()), "{message}");
}

#[test]
fn a_stop_waits_for_the_start_lock_and_then_stops() {
    let dir = state_dir("stop-wait");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start(&dir, &["--any-node"]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    let held = hold_start_lock(&dir);
    let first = helper.request_stop();
    // A second Stop doesn't cancel the first, and nothing says a start is under way.
    let second = helper.request_stop();
    std::thread::sleep(Duration::from_millis(700));
    assert!(helper.control.try_recv().is_err() && runtime.alive(), "it waits for the lock");
    assert!(call(&helper).ends_with(CALL_REPLY_ENDS), "the runtime is still reached while the stop waits");
    drop(held);
    assert_eq!(helper.next(), ToApp::Stopped { id: first });
    assert_eq!(helper.next(), ToApp::Stopped { id: second }, "each Stop is answered by its own id");
    assert!(!runtime.alive());
    std::thread::sleep(Duration::from_millis(300));
    assert!(helper.control.try_recv().is_err(), "once each");

    // A Detach while it waits is done after the stop.
    let runtime = FakeRuntime::start(&dir, "labbox3");
    helper.request_start(None, true);
    assert!(matches!(helper.next(), ToApp::Ready { .. }));
    let held = hold_start_lock(&dir);
    let stop = helper.request_stop();
    helper.send(ToHelper::Detach);
    std::thread::sleep(Duration::from_millis(700));
    assert!(runtime.alive() && helper.process.try_wait().unwrap().is_none(), "the Detach doesn't cancel the stop");
    drop(held);
    assert_eq!(helper.next(), ToApp::Stopped { id: stop }, "the Stop is answered before the helper leaves");
    helper.exits();
    assert!(!runtime.alive(), "the stop was made before the helper left");
}

#[test]
fn a_stop_that_cant_get_the_lock_says_so_and_leaves_the_runtime_attached() {
    let dir = state_dir("stop-gives-up");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia", "--any-node"], &[("ENDEAVOR_STOP_LOCK_SECS", "1")]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    let held = hold_start_lock(&dir);
    let stop = helper.request_stop();
    let ToApp::NotStopped { id, message } = helper.next() else { panic!("expected why it didn't stop") };
    assert_eq!(id, stop);
    assert!(message.contains("was not stopped") && message.contains(&dir.display().to_string()), "{message}");
    assert!(runtime.alive() && dir.join("runtime.json").exists());
    assert!(call(&helper).ends_with(CALL_REPLY_ENDS), "still attached");
    drop(held);
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    assert!(!runtime.alive());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_start_and_a_leave_said_during_a_stop_are_heard_before_the_start_runs() {
    for end_of_input in [false, true] {
        let dir = state_dir(if end_of_input { "stop-start-eof" } else { "stop-start-detach" });
        let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[]);
        helper.hello();
        let held = hold_start_lock(&dir);
        let stop = helper.request_stop();
        helper.request_start(None, true);
        if end_of_input {
            helper.stdin.0.lock().unwrap().take();
        } else {
            helper.send(ToHelper::Detach);
        }
        std::thread::sleep(Duration::from_millis(700));
        assert!(helper.control.try_recv().is_err(), "the stop waits for the lock");
        drop(held);
        assert_eq!(helper.next(), ToApp::Stopped { id: stop });
        helper.exits();
        assert!(helper.control.try_recv().is_err(), "the start was not run, so it said nothing (end of input: {end_of_input})");
        assert!(!dir.join("runtime.log").exists() && !dir.join("runtime.json").exists());
    }
}

#[test]
fn a_stop_said_after_a_start_during_a_stop_ends_that_start_and_is_answered_after_the_first_stop() {
    let dir = state_dir("stop-start-stop-cancelled");
    let helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[]);
    helper.hello();
    let held = hold_start_lock(&dir);
    let first = helper.request_stop();
    let start = helper.request_start(None, true);
    let last = helper.request_stop();
    std::thread::sleep(Duration::from_millis(500));
    drop(held);
    assert_eq!(helper.next(), ToApp::Stopped { id: first });
    assert_eq!(helper.next(), ToApp::StartCancelled { id: start });
    assert_eq!(helper.next(), ToApp::Stopped { id: last });
    assert!(!dir.join("runtime.log").exists(), "no Julia was started");
}

#[test]
fn stops_said_during_a_stop_share_its_outcome_and_wait_for_the_lock_once() {
    let dir = state_dir("stops-share");
    let helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[("ENDEAVOR_STOP_LOCK_SECS", "1")]);
    helper.hello();
    let _held = hold_start_lock(&dir);
    let ids = [helper.request_stop(), helper.request_stop(), helper.request_stop()];
    let ToApp::NotStopped { id, message } = helper.next() else { panic!("expected why it didn't stop") };
    assert_eq!(id, ids[0]);
    let began = std::time::Instant::now();
    for &id in &ids[1..] {
        assert_eq!(helper.next_within(Duration::from_millis(800)), ToApp::NotStopped { id, message: message.clone() });
    }
    assert!(began.elapsed() < Duration::from_millis(800), "the others did not wait for the lock again");
}

#[test]
fn a_detach_while_waiting_to_start_drops_an_unfinished_upload() {
    let dir = state_dir("detach-wait");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("fits")).unwrap();
    let _held = hold_start_lock(&dir);
    let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[("HOME", home.to_str().unwrap())]);
    assert!(matches!(helper.start_runtime(), ToApp::Progress { .. }));
    let ask = |id: u32, request: Request| {
        helper.send(ToHelper::Files { id, request });
        match helper.next() {
            ToApp::Files { id: got, reply } if got == id => reply,
            other => panic!("expected Files {id}, got {other:?}"),
        }
    };
    let folder = "~/fits".to_string();
    let sha256 = "b659e80980e7375313bf70ebf6e577f4abae7d5657bf7b563fa64c2f48a33eee".to_string();
    assert_eq!(ask(1, Request::Place { folder: folder.clone(), name: "decay.csv".into(), size: 8, sha256 }), Reply::Place { path: "data/decay.csv".into(), have: false });
    let unfinished = Request::Write { folder, path: "data/decay.csv".into(), offset: 0, bytes: b"t,y\n".to_vec(), last: false };
    assert_eq!(ask(2, unfinished), Reply::Written);
    let part = home.join("fits/data/.decay.csv.part");
    assert!(part.exists());
    helper.send(ToHelper::Detach);
    helper.exits();
    assert!(!part.exists(), "a Detach while it waits removes the part, as any other does");
}

#[test]
fn a_stop_holds_off_a_start_until_the_old_runtime_is_gone() {
    let dir = state_dir("stop-vs-start");
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let old = FakeRuntime::slow_to_exit(&dir, &this_host(), Duration::from_millis(1500));
    let mut a = Helper::start(&dir, &[]);
    assert!(matches!(a.start_runtime(), ToApp::Ready { reattached: true, .. }));
    let mut c = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    c.hello();

    let stop = a.request_stop();
    std::thread::sleep(Duration::from_millis(400));
    assert!(old.alive(), "it takes a moment to exit");
    c.request_start(None, true);
    let ToApp::Ready { pid, reattached, .. } = after_start(&c) else { panic!("expected Ready, not the old runtime and then its death") };
    assert_eq!(a.next(), ToApp::Stopped { id: stop });
    assert!(!old.alive());
    assert!(!reattached && pid != old.pid, "C started its own");
    assert!(common::pid_alive(pid as i32));
    std::thread::sleep(Duration::from_millis(1000));
    assert!(c.control.try_recv().is_err(), "the old runtime's end isn't C's");
    let stop = c.request_stop();
    assert_eq!(c.next(), ToApp::Stopped { id: stop });
    for helper in [&mut a, &mut c] {
        helper.stdin.0.lock().unwrap().take();
        helper.exits();
    }
}

#[test]
fn a_runtime_this_helper_may_not_stop_is_not_stopped_and_the_client_is_told() {
    let dir = state_dir("stop-other-node");
    let runtime = FakeRuntime::start(&dir, "some-other-node");
    let mut helper = Helper::start(&dir, &[]);
    helper.hello();
    let stop = helper.request_stop();
    let ToApp::NotStopped { id, message } = helper.next() else { panic!("expected NotStopped") };
    assert_eq!(id, stop);
    assert!(message.starts_with("Julia was not stopped.") && message.contains("some-other-node"), "{message}");
    assert!(runtime.alive() && dir.join("runtime.json").exists());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_start_asked_while_a_stop_waits_is_answered_after_the_stop_not_lost() {
    let dir = state_dir("stop-then-start");
    let bridge = common::FakeBridge::start(&dir);
    let julia = common::serving_julia(&dir, &bridge);
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let old = FakeRuntime::start(&dir, &this_host());
    let mut helper = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { reattached: true, .. }));
    let held = hold_start_lock(&dir);
    let stop = helper.request_stop();
    let start = helper.request_start(None, true);
    std::thread::sleep(Duration::from_millis(500));
    assert!(helper.control.try_recv().is_err() && old.alive());
    drop(held);
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    let ToApp::Ready { id, pid, reattached, .. } = after_start(&helper) else { panic!("expected Ready") };
    assert_eq!(id, start);
    assert!(!reattached && pid != old.pid, "the start began a new runtime once the old one was gone");
    let stop = helper.request_stop();
    assert_eq!(helper.next(), ToApp::Stopped { id: stop });
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn the_end_of_input_during_a_stop_that_fails_still_stops_the_runtime_with_quit_with_client() {
    let dir = state_dir("stop-fails-eof");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia", "--any-node", "--quit-with-client"], &[("ENDEAVOR_STOP_LOCK_SECS", "1")]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    let held = hold_start_lock(&dir);
    helper.request_stop();
    helper.stdin.0.lock().unwrap().take();
    assert!(matches!(helper.next(), ToApp::NotStopped { .. }));
    std::thread::sleep(Duration::from_millis(500));
    assert!(runtime.alive() && helper.process.try_wait().unwrap().is_none(), "the helper waits for the lock to stop it");
    drop(held);
    helper.exits();
    assert!(!runtime.alive(), "it was stopped as --quit-with-client says");
}

#[test]
fn a_runtime_that_is_still_alive_after_the_stop_is_not_stopped_and_stays_attached() {
    let dir = state_dir("wont-die");
    // It ignores the shutdown call, and the helper sends it no signals, as if it were stuck in the kernel.
    let runtime = FakeRuntime::slow_to_exit(&dir, "labbox3", Duration::from_secs(3600));
    let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia", "--any-node"], &[("ENDEAVOR_TEST_UNKILLABLE", "1")]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    helper.request_stop();
    let ToApp::NotStopped { message, .. } = helper.next_within(Duration::from_secs(60)) else { panic!("expected NotStopped") };
    assert!(message.contains("still running"), "{message}");
    assert!(dir.join("runtime.json").exists(), "it stays on record");
    assert!(call(&helper).ends_with(CALL_REPLY_ENDS), "its route is back");
    assert!(runtime.alive(), "it really is still running");
    runtime.kill();
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_start_asked_while_a_stop_waits_and_fails_finds_the_runtime_still_attached() {
    let dir = state_dir("stop-fails-then-start");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia", "--any-node"], &[("ENDEAVOR_STOP_LOCK_SECS", "1")]);
    let ToApp::Ready { pid: attached, .. } = helper.start_runtime() else { panic!("expected Ready") };
    let _held = hold_start_lock(&dir);
    let stop = helper.request_stop();
    let start = helper.request_start(None, true);
    assert!(matches!(helper.next(), ToApp::NotStopped { id, .. } if id == stop));
    assert!(matches!(helper.next(), ToApp::Ready { id, pid, reattached: true, .. } if id == start && pid == attached));
    assert!(runtime.alive() && call(&helper).ends_with(CALL_REPLY_ENDS));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_runtime_that_dies_while_a_stop_waits_is_reported_and_is_not_attached_again() {
    let dir = state_dir("stop-died");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia", "--any-node"], &[("ENDEAVOR_STOP_LOCK_SECS", "2")]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    let held = hold_start_lock(&dir);
    let stop = helper.request_stop();
    let start = helper.request_start(None, true);
    std::thread::sleep(Duration::from_millis(300));
    runtime.kill();
    assert!(matches!(helper.next(), ToApp::NotStopped { id, .. } if id == stop));
    drop(held);
    let died = helper.next();
    assert!(matches!(died, ToApp::Died { .. }), "the end of the runtime was heard, not lost with the wait: {died:?}");
    // The start asked for meanwhile finds nothing attached: it starts, and there is no Julia here.
    assert!(matches!(helper.after_progress(), ToApp::StartFailed { id, .. } if id == start));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

/// A helper on a "cluster" whose runtime runs in job 42 and takes a moment to
/// shut down, with a folder to upload into.
fn stopping_job(name: &str) -> (PathBuf, FakeSlurm, Helper, FakeRuntime) {
    let dir = state_dir(name);
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    helper.request_start(small_job(), true);
    assert!(matches!(helper.after_progress(), ToApp::Found { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    slurm.set("node", &this_host());
    slurm.set("state", "RUNNING");
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    let state = serde_json::json!({ "launcher": "slurm", "job": "42" });
    let runtime = FakeRuntime::start_as(&dir, &this_host(), state, Duration::from_millis(1500));
    assert!(matches!(helper.after_progress(), ToApp::Ready { .. }));
    std::fs::create_dir_all(dir.join("home/fits")).unwrap();
    (dir, slurm, helper, runtime)
}

/// An unfinished upload into `dir/home/fits`: its part.
fn unfinished_upload(helper: &Helper, dir: &Path) -> PathBuf {
    let ask = |id: u32, request: Request| {
        helper.send(ToHelper::Files { id, request });
        match helper.next() {
            ToApp::Files { id: got, reply } if got == id => reply,
            other => panic!("expected Files {id}, got {other:?}"),
        }
    };
    let folder = dir.join("home/fits").display().to_string();
    let sha256 = "b659e80980e7375313bf70ebf6e577f4abae7d5657bf7b563fa64c2f48a33eee".to_string();
    assert!(matches!(ask(1, Request::Place { folder: folder.clone(), name: "decay.csv".into(), size: 8, sha256 }), Reply::Place { .. }));
    assert_eq!(ask(2, Request::Write { folder, path: "data/decay.csv".into(), offset: 0, bytes: b"t,y\n".to_vec(), last: false }), Reply::Written);
    let part = dir.join("home/fits/data/.decay.csv.part");
    assert!(part.exists());
    part
}

#[test]
fn a_stop_then_detach_on_a_cluster_is_answered_and_leaves_nothing_behind() {
    let (dir, slurm, mut helper, runtime) = stopping_job("slurm-stop-detach");
    let part = unfinished_upload(&helper, &dir);
    let first = helper.request_stop();
    let second = helper.request_stop();
    helper.send(ToHelper::Detach);
    assert_eq!(helper.next(), ToApp::Stopped { id: first });
    assert_eq!(helper.next(), ToApp::Stopped { id: second }, "each Stop is answered, while the helper waits for the node too");
    helper.exits();
    assert!(!runtime.alive() && slurm.read("scancel.log").trim() == "42");
    assert!(!part.exists(), "the Detach removed the unfinished upload");
}

#[test]
fn a_stop_then_the_end_of_input_on_a_cluster_does_not_leave_the_helper_behind() {
    let (_dir, _slurm, mut helper, runtime) = stopping_job("slurm-stop-eof");
    helper.request_stop();
    std::thread::sleep(Duration::from_millis(300));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
    assert!(!runtime.alive());
}

/// A client `Channel` on a helper started with `flags` over `dir`.
fn channel_on(dir: &Path, flags: &[&str]) -> endeavor_mcp::client::Channel {
    let mut helper = Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .args(["connect", "--state-dir"])
        .arg(dir)
        .args(["--runtime", "/nonexistent", "--depot", "/nonexistent", "--julia", "/nonexistent/julia"])
        .args(flags)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (stdin, stdout) = (helper.stdin.take().unwrap(), helper.stdout.take().unwrap());
    let channel = endeavor_mcp::client::Channel::open(helper, stdin, stdout);
    channel.wait_hello(|| "the helper vanished".into()).unwrap();
    channel
}

fn no_helper_left(dir: &Path) {
    common::wait_for("the helper to end", || {
        let found = Command::new("pgrep").arg("-f").arg("--").arg(dir.display().to_string()).output().unwrap();
        found.stdout.is_empty()
    });
}

#[test]
fn a_channel_dropped_without_a_word_is_a_vanished_client_to_the_helper() {
    for (flags, stops) in [(&["--any-node", "--quit-with-client"][..], true), (&["--any-node"][..], false)] {
        let dir = state_dir(if stops { "channel-drop-quit" } else { "channel-drop" });
        let runtime = FakeRuntime::start(&dir, "labbox3");
        let channel = channel_on(&dir, flags);
        let listener = endeavor_mcp::client::Listener::start("lab").unwrap();
        channel.start_runtime(&listener, &endeavor_mcp::client::StartOptions::default(), &mut |_| {}, |_| {}).expect("attached");
        drop(channel);
        no_helper_left(&dir);
        assert_eq!(runtime.alive(), !stops, "the helper's own rule for a client that goes away: --quit-with-client is {stops}");
        assert_eq!(dir.join("runtime.json").exists(), !stops);
    }
}
