//! A running `endeavor-remote connect`, driven over its stdin and stdout as
//! the app drives it: control messages both ways, and relayed connections.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wire::relay::Mux;
use wire::{Target, ToApp, ToHelper};

/// A running `endeavor-remote connect`, and the app's end of its channel.
pub struct Helper {
    pub process: Child,
    pub stdin: Stdin,
    pub mux: Arc<Mux>,
    pub control: Receiver<ToApp>,
}

/// The helper's stdin, which a test can close while the mux still holds it.
#[derive(Clone)]
pub struct Stdin(pub Arc<Mutex<Option<ChildStdin>>>);

impl Write for Stdin {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.flush()
    }
}

impl Helper {
    /// Run `command` (an `endeavor-remote connect …`) with its stdin and stdout as the channel.
    pub fn spawn(mut command: Command) -> Helper {
        let mut process = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
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

    pub fn next(&self) -> ToApp {
        self.next_within(Duration::from_secs(20))
    }

    pub fn next_within(&self, wait: Duration) -> ToApp {
        self.control.recv_timeout(wait).expect("a control message")
    }

    pub fn hello(&self) -> ToApp {
        let hello = self.next();
        assert!(matches!(hello, ToApp::Hello { .. }), "{hello:?}");
        hello
    }

    /// The next message after any lines of the runtime's log.
    pub fn after_progress(&self) -> ToApp {
        loop {
            match self.next() {
                ToApp::Progress { .. } => {}
                other => return other,
            }
        }
    }

    /// Hello, then ask for the runtime: its answer.
    pub fn start_runtime(&self) -> ToApp {
        self.hello();
        self.send(ToHelper::StartRuntime { job: None });
        self.next()
    }

    pub fn send(&self, message: ToHelper) {
        self.mux.send(&message.frame()).unwrap();
    }

    pub fn exits(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.process.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "helper didn't exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A local connection relayed to `target`.
    pub fn connect(&self, target: Target) -> TcpStream {
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
