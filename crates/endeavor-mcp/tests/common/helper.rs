use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wire::relay::Mux;
use wire::{ToApp, ToHelper};

pub struct Helper {
    pub process: Child,
    pub stdin: Stdin,
    pub mux: Arc<Mux>,
    pub control: Receiver<ToApp>,
    ids: AtomicU32,
}

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
    pub fn spawn(mut command: Command) -> Helper {
        let mut process = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let stdin = Stdin(Arc::new(Mutex::new(process.stdin.take())));
        let mux = Mux::new(stdin.clone());
        let (tx, control) = mpsc::channel();
        let stdout = process.stdout.take().unwrap();
        let m = mux.clone();
        std::thread::spawn(move || {
            let _ = m.run(stdout, |_, _| {}, |json| drop(tx.send(serde_json::from_slice(json).unwrap())));
        });
        Helper { process, stdin, mux, control, ids: AtomicU32::new(1) }
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

    pub fn after_progress(&self) -> ToApp {
        loop {
            match self.next() {
                ToApp::Progress { .. } => {}
                other => return other,
            }
        }
    }

    pub fn start_runtime(&self) -> ToApp {
        self.hello();
        self.request_start(None, true);
        self.next()
    }

    pub fn request_start(&self, job: Option<wire::slurm::JobRequest>, install: bool) -> u32 {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        self.send(ToHelper::StartRuntime { id, job, engine: wire::ENGINE_PLUTO.into(), install, attach_only: false });
        id
    }

    pub fn request_attach(&self) -> u32 {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        self.send(ToHelper::StartRuntime { id, job: None, engine: wire::ENGINE_PLUTO.into(), install: false, attach_only: true });
        id
    }

    pub fn request_stop(&self) -> u32 {
        self.request_stop_as(false)
    }

    pub fn request_stop_as(&self, force: bool) -> u32 {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        self.send(ToHelper::Stop { id, force, unless_shared: false });
        id
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

    pub fn connect(&self) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        self.mux.open(listener.accept().unwrap().0).unwrap();
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
