use std::net::TcpStream;
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
    /// Set when the client ends the helper's input.
    closed: Arc<AtomicBool>,
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

struct Sent(Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>);

impl Write for Sent {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Sent {
    fn drop(&mut self) {
        self.1.store(true, Ordering::SeqCst);
    }
}

impl Scripted {
    fn new() -> Scripted {
        let (say, heard) = mpsc::channel();
        let sent: Arc<Mutex<Vec<u8>>> = Arc::default();
        let closed: Arc<AtomicBool> = Arc::default();
        let child = std::process::Command::new("true").spawn().unwrap();
        let channel = Arc::new(Channel::open(child, Sent(sent.clone(), closed.clone()), Heard(heard, Vec::new())));
        let scripted = Scripted { channel, say, sent, closed };
        scripted.tell(&ToApp::Hello { version: "0".into(), node: "n".into(), home: "/".into(), slurm: false, uploads: true });
        scripted.channel.wait_hello(|| "gone".into()).unwrap();
        scripted
    }

    fn tell(&self, message: &ToApp) {
        self.say.send(message.frame().encode()).unwrap();
    }

    /// Wait until the client has sent `n` `Stop`s.
    fn stops_sent(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while String::from_utf8_lossy(&self.sent.lock().unwrap()).matches(r#""type":"Stop""#).count() < n {
            assert!(Instant::now() < deadline, "{n} Stops were never sent");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Start the runtime, say it's ready, and call `notice` with what the
    /// watcher hears of its end.
    fn started(&self, listener: &Arc<Listener>, notice: impl FnOnce(Notice) + Send + 'static) {
        let starting = std::thread::spawn({
            let (channel, listener) = (self.channel.clone(), listener.clone());
            move || channel.start_runtime(&listener, None, &mut |_| {}, notice)
        });
        self.sent_has("StartRuntime");
        self.tell(&ready());
        starting.join().unwrap().expect("ready");
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
    assert_eq!(helper.channel.stop().unwrap_err(), "Endeavor's helper didn't answer in 1 s, so Julia may not have stopped.");
    assert!(began.elapsed() < Duration::from_millis(2500), "{:?}", began.elapsed());
    chatter.join().unwrap();
}

#[test]
fn a_late_stopped_is_not_the_next_starts_outcome() {
    let helper = Scripted::new();
    assert!(helper.channel.stop().is_err());
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
    stopping.join().unwrap().unwrap();
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
fn a_channel_nobody_holds_ends_the_helpers_input_as_a_vanished_client_does() {
    let helper = Scripted::new();
    let (sent, closed) = (helper.sent.clone(), helper.closed.clone());
    assert!(!closed.load(Ordering::SeqCst));
    drop(helper);
    assert!(closed.load(Ordering::SeqCst), "the helper sees the end of its input");
    assert!(!String::from_utf8_lossy(&sent.lock().unwrap()).contains("Detach"), "no word of its own: the helper applies its own rule");
}

#[test]
#[cfg(unix)]
fn the_exit_hook_runs_when_the_helper_has_exited_and_is_not_yet_reaped() {
    let (say, heard) = mpsc::channel::<Vec<u8>>();
    let child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id() as i32;
    let (hooked, ran) = mpsc::channel();
    let channel = Channel::open_watched(child, Sent(Arc::default(), Arc::default()), Heard(heard, Vec::new()), move || {
        // SAFETY: signal 0 only checks that the process exists; a zombie still does.
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        let _ = hooked.send(exists);
    });
    drop(say);
    assert_eq!(ran.recv_timeout(Duration::from_secs(10)), Ok(true), "its pid was still its own");
    channel.closed();
    // SAFETY: as above.
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "and it was reaped after");
}

#[test]
fn a_channel_that_was_detached_or_quit_is_left_alone_when_dropped() {
    let helper = Scripted::new();
    helper.channel.quit(true);
    helper.sent_has("Detach");
    let closed = helper.closed.clone();
    drop(helper);
    assert!(!closed.load(Ordering::SeqCst));
}

#[test]
fn a_stop_the_helper_refuses_is_an_error_and_the_runtime_stays_watched() {
    let helper = Scripted::new();
    let (noticed, heard) = mpsc::channel();
    helper.started(&Listener::start("lab").unwrap(), move |notice| drop(noticed.send(notice)));
    let stopping = std::thread::spawn({
        let channel = helper.channel.clone();
        move || channel.stop()
    });
    helper.stops_sent(1);
    helper.tell(&ToApp::NotStopped { message: "Julia was not stopped: busy.".into() });
    assert_eq!(stopping.join().unwrap(), Err("Julia was not stopped: busy.".into()));
    helper.tell(&ToApp::Died { status: "exit status: 1".into(), log_tail: Vec::new() });
    let notice = heard.recv_timeout(Duration::from_secs(5)).expect("the runtime is still watched");
    assert!(matches!(notice, Notice::Died(_)), "{notice:?}");
}

#[test]
fn a_stop_that_worked_makes_the_runtimes_end_no_notice() {
    let helper = Scripted::new();
    let (noticed, heard) = mpsc::channel();
    helper.started(&Listener::start("lab").unwrap(), move |notice| drop(noticed.send(notice)));
    let stopping = std::thread::spawn({
        let channel = helper.channel.clone();
        move || channel.stop()
    });
    helper.stops_sent(1);
    helper.tell(&ToApp::Stopped);
    stopping.join().unwrap().unwrap();
    helper.tell(&ToApp::Died { status: "exit status: 1".into(), log_tail: Vec::new() });
    assert!(heard.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn two_stops_at_once_are_each_answered_and_leave_nothing_for_the_next_start() {
    let helper = Scripted::new();
    let stop = || {
        let channel = helper.channel.clone();
        std::thread::spawn(move || channel.stop())
    };
    let (first, second) = (stop(), stop());
    helper.stops_sent(2);
    helper.tell(&ToApp::Stopped);
    helper.tell(&ToApp::Stopped);
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    let began = Instant::now();
    helper.started(&Listener::start("lab").unwrap(), |_| {});
    assert!(began.elapsed() < Duration::from_millis(900), "no answer was left over to fail the start");
}

#[test]
fn a_stop_that_was_refused_does_not_hold_up_the_next_one() {
    let helper = Scripted::new();
    let began = Instant::now();
    let stop = |n: usize| {
        let channel = helper.channel.clone();
        let stopping = std::thread::spawn(move || channel.stop());
        helper.stops_sent(n);
        stopping
    };
    let first = stop(1);
    helper.tell(&ToApp::NotStopped { message: "no lock".into() });
    assert_eq!(first.join().unwrap(), Err("no lock".into()));
    let second = stop(2);
    helper.tell(&ToApp::Stopped);
    second.join().unwrap().unwrap();
    assert!(began.elapsed() < Duration::from_millis(900), "{:?}", began.elapsed());
    helper.started(&Listener::start("lab").unwrap(), |_| {});
}

#[test]
fn a_quit_that_stops_ends_a_start_under_way() {
    let helper = Scripted::new();
    let listener = Listener::start("lab").unwrap();
    let starting = std::thread::spawn({
        let (channel, listener) = (helper.channel.clone(), listener.clone());
        move || channel.start_runtime(&listener, None, &mut |_| {}, |_| {})
    });
    helper.sent_has("StartRuntime");
    helper.channel.quit(false);
    helper.stops_sent(1);
    helper.tell(&ToApp::Stopped);
    assert_eq!(starting.join().unwrap().unwrap_err(), "Julia was stopped while it started.");
}

#[test]
fn a_stop_from_another_thread_ends_a_start_under_way() {
    let helper = Scripted::new();
    let listener = Listener::start("lab").unwrap();
    let starting = std::thread::spawn({
        let (channel, listener) = (helper.channel.clone(), listener.clone());
        move || channel.start_runtime(&listener, None, &mut |_| {}, |_| {})
    });
    helper.sent_has("StartRuntime");
    let stopping = std::thread::spawn({
        let channel = helper.channel.clone();
        move || channel.stop()
    });
    helper.stops_sent(1);
    helper.tell(&ToApp::Stopped);
    assert_eq!(starting.join().unwrap().unwrap_err(), "Julia was stopped while it started.");
    stopping.join().unwrap().unwrap();
}

#[test]
fn a_refusal_lets_the_runtime_be_watched_again_even_when_its_death_follows_at_once() {
    let helper = Scripted::new();
    let (noticed, heard) = mpsc::channel();
    helper.started(&Listener::start("lab").unwrap(), move |notice| drop(noticed.send(notice)));
    let stopping = std::thread::spawn({
        let channel = helper.channel.clone();
        move || channel.stop()
    });
    helper.stops_sent(1);
    helper.tell(&ToApp::NotStopped { message: "busy".into() });
    helper.tell(&ToApp::Died { status: "exit status: 1".into(), log_tail: Vec::new() });
    assert_eq!(stopping.join().unwrap(), Err("busy".into()));
    let notice = heard.recv_timeout(Duration::from_secs(5)).expect("the death is reported");
    assert!(matches!(notice, Notice::Died(_)), "{notice:?}");
}

#[test]
fn a_stop_that_gave_up_and_was_refused_later_leaves_the_runtime_watched() {
    let helper = Scripted::new();
    let (noticed, heard) = mpsc::channel();
    helper.started(&Listener::start("lab").unwrap(), move |notice| drop(noticed.send(notice)));
    assert!(helper.channel.stop().unwrap_err().contains("didn't answer"));
    helper.tell(&ToApp::NotStopped { message: "busy".into() });
    helper.tell(&ToApp::Died { status: "exit status: 1".into(), log_tail: Vec::new() });
    let notice = heard.recv_timeout(Duration::from_secs(5)).expect("the death is reported");
    assert!(matches!(notice, Notice::Died(_)), "{notice:?}");
}

/// What the listener says to an MCP call, which tells how it is away.
fn says(listener: &Listener) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", listener.port())).unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_notebooks"}}"#;
    write!(socket, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer t\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    response
}

fn says_within(listener: &Listener, text: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = says(listener);
        if response.contains(text) || Instant::now() > deadline {
            return response;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_helper_that_ends_unexpectedly_is_a_drop_and_after_the_client_let_it_go_is_not() {
    let listener = Listener::start("lab").unwrap();
    let helper = Scripted::new();
    helper.started(&listener, |_| {});
    let Scripted { channel, say, .. } = helper;
    drop(say);
    assert!(matches!(channel.closed(), Some(Notice::Lost(_))));
    let response = says(&listener);
    assert!(response.contains("reconnecting by itself"), "{response}");

    for how in ["quit", "detach", "drop"] {
        let listener = Listener::start("lab").unwrap();
        let helper = Scripted::new();
        helper.started(&listener, |_| {});
        let Scripted { channel, say, .. } = helper;
        match how {
            "quit" => channel.quit(true),
            "detach" => {
                let channel = channel.clone();
                std::thread::spawn(move || channel.detach());
            }
            _ => drop(channel),
        }
        std::thread::sleep(Duration::from_millis(100));
        drop(say);
        let response = says_within(&listener, "isn't connected");
        assert!(response.contains("Endeavor isn't connected to lab.") && !response.contains("reconnecting"), "{how}: {response}");
    }
}
