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
    second.send(ToHelper::Stop);
    assert_eq!(second.next(), ToApp::Stopped);
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
            ToApp::Progress { .. } | ToApp::FoundJulia { .. } | ToApp::Submitted { .. } | ToApp::Queued { .. } => {}
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
    first.send(ToHelper::StartRuntime { job: None });
    second.send(ToHelper::StartRuntime { job: None });
    let (ToApp::Ready { pid: a, reattached: a_again, .. }, ToApp::Ready { pid: b, reattached: b_again, .. }) = (after_start(&first), after_start(&second)) else { panic!("expected Ready") };
    assert_eq!(a, b, "one runtime");
    assert_eq!(running(&format!("core --state-dir {}", dir.display())), 1);
    assert!(a_again != b_again, "one started it and the other attached: {a_again} {b_again}");

    // `serve` in the same folder finds it, and hears when a helper stops it.
    let mut serve = serve_in(&dir);
    first.send(ToHelper::Stop);
    assert_eq!(first.next(), ToApp::Stopped);
    let ToApp::Died { status, .. } = second.next() else { panic!("expected Died") };
    assert_eq!(status, "It was stopped from another connection.");
    let said = ended(serve);
    assert_eq!(said.0, Some(0));
    assert!(said.1.contains(&format!("already running from {}", dir.display())), "{}", said.1);
    assert!(said.1.ends_with("Julia was stopped from another connection.\n"), "{}", said.1);
    for helper in [&mut first, &mut second] {
        helper.stdin.0.lock().unwrap().take();
        helper.exits();
    }

    // The next runtime clears the note the stop left, and `endeavor stop` tells both what it was.
    assert!(std::fs::read_to_string(dir.join("stopped")).unwrap().ends_with(" connection"));
    let mut again = Helper::start_with(&dir, &["--julia", julia.to_str().unwrap()], &[]);
    again.hello();
    again.send(ToHelper::StartRuntime { job: None });
    assert!(matches!(after_start(&again), ToApp::Ready { reattached: false, .. }));
    assert!(!dir.join("stopped").exists());
    serve = serve_in(&dir);
    let stop = Command::new(env!("CARGO_BIN_EXE_endeavor")).args(["stop", "--state-dir"]).arg(&dir).output().unwrap();
    assert!(stop.status.success());
    let ToApp::Died { status, .. } = again.next() else { panic!("expected Died") };
    assert_eq!(status, "It was stopped with `endeavor stop`.");
    let said = ended(serve);
    assert_eq!(said.0, Some(0));
    assert!(said.1.ends_with("Julia was stopped with `endeavor stop`.\n"), "{}", said.1);
    again.stdin.0.lock().unwrap().take();
    again.exits();
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
    while !line.starts_with("Ctrl-C leaves this Julia running") {
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
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
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
    helper.send(ToHelper::StartRuntime { job: None });
    assert!(matches!(helper.next(), ToApp::StartFailed { .. }));
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_runtime_recorded_on_another_node_is_not_replaced() {
    let dir = state_dir("node");
    let _runtime = FakeRuntime::start(&dir, "some-other-node");
    let mut helper = Helper::start(&dir, &[]);
    let ToApp::StartFailed { message } = helper.start_runtime() else { panic!("expected StartFailed") };
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
    let ToApp::StartFailed { message } = helper.start_runtime() else { panic!("expected StartFailed") };
    assert_eq!(message, "Julia here was started by an older version of Endeavor, which this version can't connect to. Restart Julia to use it.");
    assert_eq!(check(&helper, 1), RuntimeState::Running { node: "labbox3".into(), notebooks: None, job: None }, "it still shows as running");
    assert!(!ended.is_finished(), "nothing stopped it yet");
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
    assert!(ended.join().unwrap().signal().is_some(), "Stop ended it");
    assert!(!dir.join("runtime.json").exists());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

#[test]
fn a_runtime_that_cant_start_is_an_error() {
    let dir = state_dir("nojulia");
    let mut helper = Helper::start(&dir, &[]);
    let ToApp::StartFailed { message } = helper.start_runtime() else { panic!("expected StartFailed") };
    assert!(message.contains("/nonexistent/julia"), "{message}");
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}

/// A julia that says it's 1.12 and then fails to boot.
fn fake_julia(dir: &Path) -> PathBuf {
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let julia = bin.join("julia");
    std::fs::write(&julia, "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'julia version 1.12.0'; exit 0; }\necho 'ERROR: boom'\nexit 3\n").unwrap();
    std::fs::set_permissions(&julia, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    julia
}

#[test]
fn julia_from_a_shell_line_is_found_and_its_failure_reported() {
    let dir = state_dir("shell-julia");
    let julia = fake_julia(&dir);
    let line = format!("PATH={}:$PATH", julia.parent().unwrap().display());
    let mut helper = Helper::start_with(&dir, &["--julia-shell", &line], &[("SHELL", "/bin/sh")]);
    assert_eq!(helper.start_runtime(), ToApp::FoundJulia { path: julia.display().to_string(), version: "1.12.0".into() });
    let ToApp::Died { status, log_tail } = helper.after_progress() else { panic!("expected Died") };
    assert!(status.contains('3'), "{status}");
    assert_eq!(log_tail, ["ERROR: boom"]);
    helper.stdin.0.lock().unwrap().take();
    helper.exits();

    // A login shell's profile can put a julia back on PATH (GitHub's Ubuntu image has one), so the line empties it.
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let line = format!("PATH={}", empty.display());
    let mut helper = Helper::start_with(&dir, &["--julia-shell", &line], &[("SHELL", "/bin/sh")]);
    let ToApp::StartFailed { message } = helper.start_runtime() else { panic!("expected StartFailed") };
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
    assert!(matches!(helper.start_runtime(), ToApp::FoundJulia { .. }));
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

    // Both go to the core: the call on to Julia's bridge, the WebSocket to Pluto.
    let mut call = helper.connect();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"endeavor/set_folder","params":{"path":"/n"}}"#;
    write!(call, "POST /endeavor/call HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut reply = String::new();
    call.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200") && reply.contains(r#""said":"POST /call HTTP/1.0""#), "{reply}");
    let mut pluto = websocket(&helper);
    pluto.write_all(b"to Pluto").unwrap();
    let mut back = [0; 8];
    pluto.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"to Pluto");

    // Stop reaches Julia through the core, and neither is left.
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
    assert!(!common::pid_alive(core) && !common::pid_alive(julia));
    assert!(!dir.join("runtime.json").exists() && !dir.join("julia.json").exists());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
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
    helper.send(ToHelper::StartRuntime { job: small_job() });
    assert!(matches!(helper.after_progress(), ToApp::FoundJulia { .. }));
    assert_eq!(helper.next(), ToApp::Submitted { job: "42".into(), summary: "2 CPUs · 8 GB · 30 min".into() });
    assert_eq!(helper.next(), ToApp::Queued { job: "42".into(), state: "PENDING".into(), reason: "Priority".into() });
    let sbatch = slurm.read("sbatch.args");
    assert!(sbatch.contains("--parsable --job-name=endeavor"), "{sbatch}");
    assert!(sbatch.contains("--account=lab --partition=short --cpus-per-task=2 --mem=8G --time=30"), "{sbatch}");
    assert!(sbatch.contains(&format!("--output={}", dir.join("runtime.log").display())), "{sbatch}");
    let script = slurm.read("job.sh");
    assert!(script.contains("node-start") && script.contains("--depot '/scratch/jc/endeavor/depot:' --build '1.0.0-abc'"), "{script}");
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
    helper.send(ToHelper::StartRuntime { job: None });
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
    helper.send(ToHelper::StartRuntime { job: small_job() });
    assert!(matches!(helper.after_progress(), ToApp::FoundJulia { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    helper.send(ToHelper::Detach);
    helper.exits();
    assert_eq!(slurm.read("scancel.log"), "", "leaving doesn't cancel");

    // The next connect waits on the same job instead of submitting another.
    let helper = slurm.helper(&dir, &julia);
    assert_eq!(helper.start_runtime(), ToApp::Submitted { job: "42".into(), summary: "2 CPUs · 8 GB · 30 min".into() });
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    assert_eq!(slurm.read("sbatch.args").lines().count(), 1);
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
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
    helper.send(ToHelper::StartRuntime { job: small_job() });
    assert!(matches!(helper.after_progress(), ToApp::FoundJulia { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    std::fs::write(dir.join("runtime.log"), "ERROR: out of disk quota\n").unwrap();
    slurm.set("sacct", "FAILED");
    slurm.set("state", "FAILED");
    let ToApp::StartFailed { message } = helper.after_progress() else { panic!("expected StartFailed") };
    assert_eq!(message, "Its Slurm job failed. Julia wasn't ready yet. Its last output: ERROR: out of disk quota");
}

#[test]
fn stopping_a_running_job_shuts_julia_down_then_cancels_the_job() {
    let dir = state_dir("slurm-stop");
    let julia = fake_julia(&dir);
    let slurm = FakeSlurm::new(&dir);
    let helper = slurm.helper(&dir, &julia);
    helper.hello();
    helper.send(ToHelper::StartRuntime { job: small_job() });
    assert!(matches!(helper.after_progress(), ToApp::FoundJulia { .. }));
    assert!(matches!(helper.next(), ToApp::Submitted { .. }));
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    slurm.set("node", &this_host());
    slurm.set("state", "RUNNING");
    assert!(matches!(helper.next(), ToApp::Queued { .. }));
    let runtime = FakeRuntime::in_job(&dir, &this_host(), "42");
    assert!(matches!(helper.after_progress(), ToApp::Ready { .. }));

    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
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
    first.send(ToHelper::StartRuntime { job: small_job() });
    assert!(matches!(first.after_progress(), ToApp::FoundJulia { .. }));
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

    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
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
    first.send(ToHelper::StartRuntime { job: small_job() });
    second.send(ToHelper::StartRuntime { job: small_job() });
    for helper in [&first, &second] {
        loop {
            match helper.next() {
                ToApp::Queued { job, .. } => break assert_eq!(job, "42"),
                ToApp::Progress { .. } | ToApp::FoundJulia { .. } | ToApp::Submitted { .. } => {}
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

    first.send(ToHelper::Stop);
    assert_eq!(first.next(), ToApp::Stopped);
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
            ToApp::Progress { .. } | ToApp::FoundJulia { .. } | ToApp::Submitted { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
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
    a.send(ToHelper::StartRuntime { job: small_job() });
    assert_eq!(queued(&a), "42");
    b.send(ToHelper::StartRuntime { job: small_job() });
    assert_eq!(queued(&b), "42");

    // A stops the job and asks again, which submits the next one, before B looks again.
    a.send(ToHelper::Stop);
    assert_eq!(a.next(), ToApp::Stopped);
    slurm.set("next", "43");
    a.send(ToHelper::StartRuntime { job: small_job() });
    assert_eq!(queued(&a), "43");
    let ToApp::StartFailed { message } = b.next() else { panic!("expected StartFailed") };
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
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
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
    helper.send(ToHelper::StartRuntime { job: small_job() });
    assert_eq!(queued(&helper), "42");
    assert!(xdg.join("endeavor/cluster/job.json").exists(), "a cluster's folder has no host name in it");
    assert!(!xdg.join("endeavor/serve").exists());

    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.args(["connect", "--julia", "/nonexistent/julia", "--runtime", "/nonexistent", "--depot", "/nonexistent"]).env("HOME", &home).env("XDG_STATE_HOME", &xdg);
    let helper = Helper::spawn(command);
    helper.hello();
    helper.send(ToHelper::StartRuntime { job: None });
    assert!(matches!(helper.next(), ToApp::StartFailed { .. }));
    assert!(xdg.join("endeavor/serve").join(this_host()).join("start.lock").exists());
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
    helper.send(ToHelper::StartRuntime { job: None });
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

    // And Stop, which starts nothing.
    let helper = Helper::start(&dir, &[]);
    assert_eq!(helper.start_runtime(), ToApp::Progress { line: line.into() });
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
    assert!(!dir.join("runtime.json").exists());

    // A holder that never lets go is given up on.
    let helper = Helper::start_with(&dir, &["--julia", "/nonexistent/julia"], &[("ENDEAVOR_START_LOCK_SECS", "1")]);
    assert_eq!(helper.start_runtime(), ToApp::Progress { line: line.into() });
    let ToApp::StartFailed { message } = helper.next() else { panic!("expected StartFailed") };
    assert!(message.contains("Gave up waiting") && message.contains(&dir.display().to_string()), "{message}");
}

#[test]
fn a_stop_waits_for_the_start_lock_and_then_stops() {
    let dir = state_dir("stop-wait");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start(&dir, &["--any-node"]);
    assert!(matches!(helper.start_runtime(), ToApp::Ready { .. }));
    let held = hold_start_lock(&dir);
    helper.send(ToHelper::Stop);
    // A second Stop doesn't cancel the first, and nothing says a start is under way.
    helper.send(ToHelper::Stop);
    std::thread::sleep(Duration::from_millis(700));
    assert!(helper.control.try_recv().is_err() && runtime.alive(), "it waits for the lock");
    assert!(call(&helper).ends_with(CALL_REPLY_ENDS), "the runtime is still reached while the stop waits");
    drop(held);
    assert_eq!(helper.next(), ToApp::Stopped);
    assert_eq!(helper.next(), ToApp::Stopped, "each Stop is answered");
    assert!(!runtime.alive());
    std::thread::sleep(Duration::from_millis(300));
    assert!(helper.control.try_recv().is_err(), "once each");

    // A Detach while it waits is done after the stop.
    let runtime = FakeRuntime::start(&dir, "labbox3");
    helper.send(ToHelper::StartRuntime { job: None });
    assert!(matches!(helper.next(), ToApp::Ready { .. }));
    let held = hold_start_lock(&dir);
    helper.send(ToHelper::Stop);
    helper.send(ToHelper::Detach);
    std::thread::sleep(Duration::from_millis(700));
    assert!(runtime.alive() && helper.process.try_wait().unwrap().is_none(), "the Detach doesn't cancel the stop");
    drop(held);
    assert_eq!(helper.next(), ToApp::Stopped, "the Stop is answered before the helper leaves");
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
    helper.send(ToHelper::Stop);
    let ToApp::NotStopped { message } = helper.next() else { panic!("expected why it didn't stop") };
    assert!(message.contains("was not stopped") && message.contains(&dir.display().to_string()), "{message}");
    assert!(runtime.alive() && dir.join("runtime.json").exists());
    assert!(call(&helper).ends_with(CALL_REPLY_ENDS), "still attached");
    drop(held);
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
    assert!(!runtime.alive());
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
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

    a.send(ToHelper::Stop);
    std::thread::sleep(Duration::from_millis(400));
    assert!(old.alive(), "it takes a moment to exit");
    c.send(ToHelper::StartRuntime { job: None });
    let ToApp::Ready { pid, reattached, .. } = after_start(&c) else { panic!("expected Ready, not the old runtime and then its death") };
    assert_eq!(a.next(), ToApp::Stopped);
    assert!(!old.alive());
    assert!(!reattached && pid != old.pid, "C started its own");
    assert!(common::pid_alive(pid as i32));
    std::thread::sleep(Duration::from_millis(1000));
    assert!(c.control.try_recv().is_err(), "the old runtime's end isn't C's");
    c.send(ToHelper::Stop);
    assert_eq!(c.next(), ToApp::Stopped);
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
    helper.send(ToHelper::Stop);
    let ToApp::NotStopped { message } = helper.next() else { panic!("expected NotStopped") };
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
    helper.send(ToHelper::Stop);
    helper.send(ToHelper::StartRuntime { job: None });
    std::thread::sleep(Duration::from_millis(500));
    assert!(helper.control.try_recv().is_err() && old.alive());
    drop(held);
    assert_eq!(helper.next(), ToApp::Stopped);
    let ToApp::Ready { pid, reattached, .. } = after_start(&helper) else { panic!("expected Ready") };
    assert!(!reattached && pid != old.pid, "the start began a new runtime once the old one was gone");
    helper.send(ToHelper::Stop);
    assert_eq!(helper.next(), ToApp::Stopped);
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
    helper.send(ToHelper::Stop);
    helper.send(ToHelper::StartRuntime { job: None });
    assert!(matches!(helper.next(), ToApp::NotStopped { .. }));
    assert!(matches!(helper.next(), ToApp::Ready { pid, reattached: true, .. } if pid == attached));
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
    helper.send(ToHelper::Stop);
    helper.send(ToHelper::StartRuntime { job: None });
    std::thread::sleep(Duration::from_millis(300));
    runtime.kill();
    assert!(matches!(helper.next(), ToApp::NotStopped { .. }));
    drop(held);
    let died = helper.next();
    assert!(matches!(died, ToApp::Died { .. }), "the end of the runtime was heard, not lost with the wait: {died:?}");
    // The start asked for meanwhile finds nothing attached: it starts, and there is no Julia here.
    assert!(matches!(helper.after_progress(), ToApp::StartFailed { .. }));
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
    helper.send(ToHelper::StartRuntime { job: small_job() });
    assert!(matches!(helper.after_progress(), ToApp::FoundJulia { .. }));
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
    helper.send(ToHelper::Stop);
    helper.send(ToHelper::Stop);
    helper.send(ToHelper::Detach);
    assert_eq!(helper.next(), ToApp::Stopped);
    assert_eq!(helper.next(), ToApp::Stopped, "each Stop is answered, while the helper waits for the node too");
    helper.exits();
    assert!(!runtime.alive() && slurm.read("scancel.log").trim() == "42");
    assert!(!part.exists(), "the Detach removed the unfinished upload");
}

#[test]
fn a_stop_then_the_end_of_input_on_a_cluster_does_not_leave_the_helper_behind() {
    let (_dir, _slurm, mut helper, runtime) = stopping_job("slurm-stop-eof");
    helper.send(ToHelper::Stop);
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
        channel.start_runtime(&listener, None, &mut |_| {}, |_| {}).expect("attached");
        drop(channel);
        no_helper_left(&dir);
        assert_eq!(runtime.alive(), !stops, "the helper's own rule for a client that goes away: --quit-with-client is {stops}");
        assert_eq!(dir.join("runtime.json").exists(), !stops);
    }
}
