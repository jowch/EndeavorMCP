//! The helper binary against a stand-in runtime (a `sleep` process plus two
//! small TCP servers on loopback), so no Julia is needed: file requests before
//! any runtime, attaching on request, relaying, one client at a time, and each
//! way a connection ends.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wire::files::{Reply, Request};
use wire::relay::Mux;
use wire::{Target, ToApp, ToHelper};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A runtime as the helper sees one: a live pid, a Pluto port (echo here) and a
/// bridge that checks the token and exits the "runtime" on `endeavor/shutdown`.
struct FakeRuntime {
    process: Arc<Mutex<Child>>,
    pid: u32,
}

impl FakeRuntime {
    fn start(dir: &Path, node: &str) -> FakeRuntime {
        let process = Command::new("sleep").arg("600").process_group(0).spawn().unwrap();
        let pid = process.id();
        let process = Arc::new(Mutex::new(process));
        let pluto = serve(|mut socket| {
            let mut reader = socket.try_clone().unwrap();
            let _ = std::io::copy(&mut reader, &mut socket);
        });
        let p = process.clone();
        let bridge = serve(move |socket| bridge(socket, &p));
        let state = serde_json::json!({
            "launcher": "process", "node": node, "pid": pid, "pluto_port": pluto, "mcp_port": bridge,
            "token": TOKEN, "pluto_secret": "s3cret",
        });
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

fn bridge(mut socket: TcpStream, process: &Mutex<Child>) {
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
    if auth != format!("Bearer {TOKEN}") {
        let _ = socket.write_all(b"HTTP/1.1 401 Unauthorized\r\n\r\n");
        return;
    }
    if String::from_utf8_lossy(&body).contains("endeavor/shutdown") {
        let mut process = process.lock().unwrap();
        let _ = process.kill();
        let _ = process.wait();
    }
    let _ = write!(socket, "HTTP/1.1 200 OK\r\n\r\n{{\"said\":{:?}}}", request.trim_end());
}

/// A running `endeavor-remote connect`, and the app's end of its channel.
struct Helper {
    process: Child,
    stdin: Stdin,
    mux: Arc<Mux>,
    control: Receiver<ToApp>,
}

/// The helper's stdin, which a test can close while the mux still holds it.
#[derive(Clone)]
struct Stdin(Arc<Mutex<Option<ChildStdin>>>);

impl Write for Stdin {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.flush()
    }
}

impl Helper {
    fn start(dir: &Path, flags: &[&str]) -> Helper {
        let flags = [&["--julia", "/nonexistent/julia"], flags].concat();
        Helper::start_with(dir, &flags, &[])
    }

    fn start_with(dir: &Path, flags: &[&str], env: &[(&str, &str)]) -> Helper {
        let mut process = Command::new(env!("CARGO_BIN_EXE_endeavor-remote"))
            .args(["connect", "--state-dir"])
            .arg(dir)
            .args(["--runtime", "/nonexistent", "--depot", "/nonexistent"])
            .args(flags)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = Stdin(Arc::new(Mutex::new(process.stdin.take())));
        let mux = Mux::new(stdin.clone());
        let (tx, control) = mpsc::channel();
        let stdout = process.stdout.take().unwrap();
        let m = mux.clone();
        std::thread::spawn(move || {
            let _ = m.run(stdout, |_, _, _| {}, |json| drop(tx.send(serde_json::from_slice(json).unwrap())));
        });
        Helper { process, stdin, mux, control }
    }

    fn next(&self) -> ToApp {
        self.control.recv_timeout(Duration::from_secs(20)).expect("a control message")
    }

    fn hello(&self) -> ToApp {
        let hello = self.next();
        assert!(matches!(hello, ToApp::Hello { .. }), "{hello:?}");
        hello
    }

    /// The next message after any lines of the runtime's log.
    fn after_progress(&self) -> ToApp {
        loop {
            match self.next() {
                ToApp::Progress { .. } => {}
                other => return other,
            }
        }
    }

    /// Hello, then ask for the runtime: its answer.
    fn start_runtime(&self) -> ToApp {
        self.hello();
        self.send(ToHelper::StartRuntime);
        self.next()
    }

    fn send(&self, message: ToHelper) {
        self.mux.send(&message.frame()).unwrap();
    }

    fn exits(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.process.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "helper didn't exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A local connection relayed to `target`.
    fn connect(&self, target: Target) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        self.mux.open(target, listener.accept().unwrap().0).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        client
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn state_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("endeavor-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn attaches_relays_hands_over_and_stops() {
    let dir = state_dir("attach");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut first = Helper::start(&dir, &["--any-node"]);
    let ToApp::Ready { launcher, node, pid, token, pluto_secret, reattached } = first.start_runtime() else { unreachable!() };
    assert_eq!((launcher.as_str(), node.as_str(), pid), ("process", "labbox3", runtime.pid));
    assert_eq!((token.as_str(), pluto_secret.as_str(), reattached), (TOKEN, "s3cret", true));

    // Pluto's port echoes; the bridge answers HTTP and closes.
    let mut pluto = first.connect(Target::Pluto);
    pluto.write_all(b"over the relay").unwrap();
    let mut back = [0; 14];
    pluto.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"over the relay");
    let mut call = first.connect(Target::Bridge);
    write!(call, "POST /call HTTP/1.0\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}").unwrap();
    let mut reply = String::new();
    call.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("{\"said\":\"POST /call HTTP/1.0\"}"), "{reply}");

    // A second client only connecting takes nothing; asking for the runtime takes
    // it over: the first hears why and exits, its streams closed.
    let mut second = Helper::start(&dir, &["--any-node", "--quit-with-client"]);
    second.hello();
    std::thread::sleep(Duration::from_millis(300));
    assert!(first.control.try_recv().is_err(), "connecting alone replaced the first client");
    second.send(ToHelper::StartRuntime);
    assert_eq!(first.next(), ToApp::Replaced);
    first.exits();
    let mut rest = Vec::new();
    assert_eq!(pluto.read_to_end(&mut rest).unwrap_or(0), 0);
    let ToApp::Ready { reattached, .. } = second.next() else { unreachable!() };
    assert!(reattached);
    assert!(runtime.alive(), "handing over doesn't stop the runtime");

    // Stop shuts the runtime down through its bridge and clears the state; the
    // helper stays connected until the app goes.
    second.send(ToHelper::Stop);
    assert_eq!(second.next(), ToApp::Stopped);
    assert!(!runtime.alive());
    assert!(!dir.join("runtime.json").exists());
    assert!(second.process.try_wait().unwrap().is_none(), "a stop keeps the helper");
    second.stdin.0.lock().unwrap().take();
    second.exits();
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
    assert!(!dir.join("lock").exists(), "no runtime was asked for, so no lock");
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
    helper.send(ToHelper::StartRuntime);
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

    let mut helper = Helper::start_with(&dir, &["--julia-shell", "true"], &[("SHELL", "/bin/sh"), ("PATH", "/usr/bin:/bin")]);
    let ToApp::StartFailed { message } = helper.start_runtime() else { panic!("expected StartFailed") };
    assert!(message.contains("`true`") && message.contains("PATH"), "{message}");
    helper.stdin.0.lock().unwrap().take();
    helper.exits();
}
