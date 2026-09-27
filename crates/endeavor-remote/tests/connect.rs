//! The helper binary against a stand-in runtime (a `sleep` process plus two
//! small TCP servers on loopback), so no Julia is needed: attaching, relaying,
//! one client at a time, and each way a connection ends.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
        let mut process = Command::new(env!("CARGO_BIN_EXE_endeavor-remote"))
            .args(["connect", "--state-dir"])
            .arg(dir)
            .args(["--julia", "/nonexistent/julia", "--runtime", "/nonexistent", "--depot", "/nonexistent"])
            .args(flags)
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
    let ToApp::Hello { launcher, node, pid, token, pluto_secret, reattached, .. } = first.hello() else { unreachable!() };
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

    // A second client takes over; the first hears why and exits, its streams closed.
    let second = Helper::start(&dir, &["--any-node", "--quit-with-client"]);
    assert_eq!(first.next(), ToApp::Replaced);
    first.exits();
    let mut rest = Vec::new();
    assert_eq!(pluto.read_to_end(&mut rest).unwrap_or(0), 0);
    let ToApp::Hello { reattached, .. } = second.hello() else { unreachable!() };
    assert!(reattached);
    assert!(runtime.alive(), "handing over doesn't stop the runtime");

    // Stop shuts the runtime down through its bridge and clears the state.
    let mut second = second;
    second.send(ToHelper::Stop);
    second.exits();
    assert!(!runtime.alive());
    assert!(!dir.join("runtime.json").exists());
}

#[test]
fn detaching_leaves_the_runtime_and_quit_with_client_stops_it_on_eof() {
    let dir = state_dir("detach");
    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start(&dir, &["--any-node", "--quit-with-client"]);
    helper.hello();
    helper.send(ToHelper::Detach);
    helper.exits();
    // --quit-with-client makes Detach a Stop.
    assert!(!runtime.alive());

    let runtime = FakeRuntime::start(&dir, "labbox3");
    let mut helper = Helper::start(&dir, &["--any-node"]);
    helper.hello();
    helper.send(ToHelper::Detach);
    helper.exits();
    assert!(runtime.alive() && dir.join("runtime.json").exists());

    let mut helper = Helper::start(&dir, &["--any-node", "--quit-with-client"]);
    helper.hello();
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
    helper.hello();
    runtime.kill();
    let ToApp::Died { status, log_tail } = helper.next() else { panic!("expected Died") };
    assert_eq!(status, "exited");
    assert_eq!(log_tail, ["booting", "Go to http://localhost:1234/?secret=… now", "ERROR: boom"]);
    helper.exits();
    assert!(!dir.join("runtime.json").exists());
}

#[test]
fn a_runtime_recorded_on_another_node_is_not_replaced() {
    let dir = state_dir("node");
    let _runtime = FakeRuntime::start(&dir, "some-other-node");
    let mut helper = Helper::start(&dir, &[]);
    let ToApp::Error { message } = helper.next() else { panic!("expected Error") };
    assert!(message.contains("some-other-node"), "{message}");
    helper.exits();
    assert!(dir.join("runtime.json").exists());
}

#[test]
fn a_runtime_that_cant_start_is_an_error() {
    let dir = state_dir("nojulia");
    let mut helper = Helper::start(&dir, &[]);
    let ToApp::Error { message } = helper.next() else { panic!("expected Error") };
    assert!(message.contains("/nonexistent/julia"), "{message}");
    helper.exits();
    assert!(dir.join("token").exists());
}
