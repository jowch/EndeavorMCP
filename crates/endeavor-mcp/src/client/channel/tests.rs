use std::time::Instant;

use super::*;

#[test]
fn says_plainly_why_julia_stopped() {
    assert_eq!(died_reason("exited", &[]), "");
    assert_eq!(died_reason("exit status: 3", &[]), "It exited with code 3.");
    assert_eq!(died_reason("signal: 9 (SIGKILL)", &[]), "It was killed (signal 9 (SIGKILL)), perhaps for using too much memory.");
    assert_eq!(died_reason("Its Slurm job reached its time limit.", &[]), "Its Slurm job reached its time limit.");
    assert_eq!(died_reason("It was stopped from another connection.", &[]), "It was stopped from another connection.");
    assert!(died_reason("exited", &["IOError: listen: address already in use (EADDRINUSE)".into()]).contains("port"));
}

#[test]
fn diagnoses_common_failures_or_shows_the_log() {
    let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
    assert!(diagnose(&lines("ERROR: Could not resolve host: github.com")).contains("internet"));
    assert!(diagnose(&lines("ERROR: Unsatisfiable requirements detected")).contains("conflict"));
    assert!(diagnose(&lines("IOError: listen: address already in use (EADDRINUSE)")).contains("port"));
    let other = diagnose(&lines("ERROR: LoadError: boom\nStacktrace: …"));
    assert!(other.starts_with("Last output:") && other.contains("boom"));
    assert_eq!(diagnose(&[]), "It printed nothing.");
}

/// The helper's end of a channel, scripted: what it says is pushed with `say`, and what the client sent is `sent`.
struct Scripted {
    channel: Arc<Channel>,
    say: mpsc::Sender<Vec<u8>>,
    sent: Arc<Mutex<Vec<u8>>>,
}

struct Heard(mpsc::Receiver<Vec<u8>>, Vec<u8>);

impl Read for Heard {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.1.is_empty() {
            match self.0.recv() {
                Ok(bytes) => self.1 = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.1.len());
        buf[..n].copy_from_slice(&self.1[..n]);
        self.1.drain(..n);
        Ok(n)
    }
}

struct Sent(Arc<Mutex<Vec<u8>>>);

impl Write for Sent {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Scripted {
    fn new() -> Scripted {
        let (say, heard) = mpsc::channel();
        let sent: Arc<Mutex<Vec<u8>>> = Arc::default();
        let child = std::process::Command::new("true").spawn().unwrap();
        let channel = Arc::new(Channel::open(child, Sent(sent.clone()), Heard(heard, Vec::new())));
        let scripted = Scripted { channel, say, sent };
        scripted.tell(&ToApp::Hello { version: "0".into(), node: "n".into(), home: "/".into(), slurm: false, uploads: true });
        scripted.channel.wait_hello(|| "gone".into()).unwrap();
        scripted
    }

    fn tell(&self, message: &ToApp) {
        self.say.send(message.frame().encode()).unwrap();
    }

    /// Wait until the client has sent a frame holding `text`.
    fn sent_has(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&self.sent.lock().unwrap()).contains(text) {
            assert!(Instant::now() < deadline, "{text} was never sent");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn ready() -> ToApp {
    ToApp::Ready { launcher: "process".into(), node: "n".into(), pid: 1, token: "t".into(), reattached: false, job: None }
}

#[test]
fn a_stop_waits_for_one_deadline_however_much_the_helper_says() {
    let helper = Scripted::new();
    let say = helper.say.clone();
    let chatter = std::thread::spawn(move || {
        for _ in 0..30 {
            let _ = say.send(ToApp::Progress { line: "x".into() }.frame().encode());
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let began = Instant::now();
    helper.channel.stop();
    assert!(began.elapsed() < Duration::from_millis(2500), "{:?}", began.elapsed());
    chatter.join().unwrap();
}

#[test]
fn a_late_stopped_is_not_the_next_starts_outcome() {
    let helper = Scripted::new();
    helper.channel.stop();
    let listener = Listener::start("lab").unwrap();
    let starting = std::thread::spawn({
        let (channel, listener) = (helper.channel.clone(), listener.clone());
        move || channel.start_runtime(&listener, None, &mut |_| {}, |_| {})
    });
    helper.sent_has("StartRuntime");
    helper.tell(&ToApp::Stopped);
    helper.tell(&ready());
    let runtime = starting.join().unwrap().expect("the late Stopped was for the stop");
    assert_eq!(runtime.token, "t");
}

#[test]
fn a_stopped_that_arrives_in_time_is_not_held_against_the_next_start() {
    let helper = Scripted::new();
    let stopping = std::thread::spawn({
        let channel = helper.channel.clone();
        move || channel.stop()
    });
    helper.sent_has("Stop");
    helper.tell(&ToApp::Stopped);
    stopping.join().unwrap();
    let listener = Listener::start("lab").unwrap();
    let starting = std::thread::spawn({
        let (channel, listener) = (helper.channel.clone(), listener.clone());
        move || channel.start_runtime(&listener, None, &mut |_| {}, |_| {})
    });
    helper.sent_has("StartRuntime");
    helper.tell(&ToApp::Stopped);
    assert_eq!(starting.join().unwrap().unwrap_err(), "Julia was stopped while it started.");
}

#[test]
fn a_channel_nobody_holds_lets_the_helper_go() {
    let helper = Scripted::new();
    let sent = helper.sent.clone();
    drop(helper);
    assert!(String::from_utf8_lossy(&sent.lock().unwrap()).contains("Detach"));
}
