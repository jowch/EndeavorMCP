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
        scripted.tell(&ToApp::Hello { protocol: wire::PROTOCOL, version: "0".into(), node: "n".into(), home: "/".into(), slurm: false, uploads: true, launcher: "process".into() });
        scripted.channel.wait_hello(|| "gone".into()).unwrap();
        scripted
    }

    fn tell(&self, message: &ToApp) {
        self.say.send(message.frame().encode()).unwrap();
    }

    /// Every control message the client has sent so far.
    fn requests(&self) -> Vec<ToHelper> {
        let bytes = self.sent.lock().unwrap().clone();
        let mut bytes = &bytes[..];
        let mut out = Vec::new();
        while let Ok(Some(Frame::Control(json))) = Frame::read_from(&mut bytes) {
            out.push(serde_json::from_slice(&json).unwrap());
        }
        out
    }

    /// Wait until the client has sent `n` requests that `pick` takes: their ids.
    fn ids(&self, n: usize, pick: fn(&ToHelper) -> Option<u32>) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ids: Vec<u32> = self.requests().iter().filter_map(pick).collect();
            if ids.len() >= n {
                return ids;
            }
            assert!(Instant::now() < deadline, "{n} requests were never sent");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the client has sent `n` `Stop`s: their ids.
    fn stops_sent(&self, n: usize) -> Vec<u32> {
        self.ids(n, |request| if let ToHelper::Stop { id } = request { Some(*id) } else { None })
    }

    /// Wait until the client has sent `n` `StartRuntime`s: their ids.
    fn starts_sent(&self, n: usize) -> Vec<u32> {
        self.ids(n, |request| if let ToHelper::StartRuntime { id, .. } = request { Some(*id) } else { None })
    }

    /// Start the runtime, say it's ready, and call `notice` with what the
    /// watcher hears of its end.
    fn started(&self, listener: &Arc<Listener>, notice: impl FnOnce(Notice) + Send + 'static) {
        let starting = self.starting(listener, notice);
        let id = *self.starts_sent(1).last().unwrap();
        self.tell(&ready(id));
        starting.join().unwrap().expect("ready");
    }

    /// A start on its own thread.
    fn starting(&self, listener: &Arc<Listener>, notice: impl FnOnce(Notice) + Send + 'static) -> std::thread::JoinHandle<Result<Runtime, String>> {
        std::thread::spawn({
            let (channel, listener) = (self.channel.clone(), listener.clone());
            move || channel.start_runtime(&listener, &StartOptions::default(), &mut |_| {}, notice).map_err(StartError::message)
        })
    }

    /// A stop on its own thread.
    fn stopping(&self) -> std::thread::JoinHandle<Result<(), String>> {
        let channel = self.channel.clone();
        std::thread::spawn(move || channel.stop())
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

fn ready(id: u32) -> ToApp {
    ready_with(id, "t")
}

fn ready_with(id: u32, token: &str) -> ToApp {
    ToApp::Ready { id, launcher: "process".into(), node: "n".into(), pid: 1, token: token.into(), reattached: false, job: None, port: Some(4000), build: None, interface: None }
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
fn a_late_answer_for_an_abandoned_stop_is_dropped_and_is_not_the_next_requests() {
    let helper = Scripted::new();
    assert!(helper.channel.stop().is_err());
    let [abandoned] = helper.stops_sent(1)[..] else { panic!() };

    let listener = Listener::start("lab").unwrap();
    let starting = helper.starting(&listener, |_| {});
    let [start] = helper.starts_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::Stopped { id: abandoned });
    helper.tell(&ready(start));
    let runtime = starting.join().unwrap().expect("the late Stopped was for the stop");
    assert_eq!(runtime.token, "t");

    let stopping = helper.stopping();
    let [_, next] = helper.stops_sent(2)[..] else { panic!() };
    assert_ne!(next, abandoned);
    helper.tell(&ToApp::NotStopped { id: abandoned, message: "late".into() });
    std::thread::sleep(Duration::from_millis(200));
    assert!(!stopping.is_finished(), "the late answer is not the next stop's");
    helper.tell(&ToApp::Stopped { id: next });
    stopping.join().unwrap().unwrap();
}

#[test]
fn a_start_that_a_stop_cut_short_ends_with_its_own_answer() {
    let helper = Scripted::new();
    let stopping = helper.stopping();
    let [stop] = helper.stops_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::Stopped { id: stop });
    stopping.join().unwrap().unwrap();
    let starting = helper.starting(&Listener::start("lab").unwrap(), |_| {});
    let [start] = helper.starts_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::StartCancelled { id: start });
    assert_eq!(starting.join().unwrap().unwrap_err(), "Julia was stopped while it started.");
}

#[test]
fn the_answers_of_two_starts_are_not_confused() {
    let helper = Scripted::new();
    let listener = Listener::start("lab").unwrap();

    // The first ended, and a stray answer for it comes during the next.
    let first = helper.starting(&listener, |_| {});
    let [one] = helper.starts_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::StartFailed { id: one, message: "no".into() });
    assert_eq!(first.join().unwrap().unwrap_err(), "no");
    let second = helper.starting(&listener, |_| {});
    let [_, two] = helper.starts_sent(2)[..] else { panic!() };
    helper.tell(&ToApp::StartFailed { id: one, message: "stray".into() });
    helper.tell(&ready_with(two, "two"));
    assert_eq!(second.join().unwrap().unwrap().token, "two");
}

#[test]
fn a_second_start_while_one_waits_fails_at_once_and_leaves_the_first_alone() {
    let helper = Scripted::new();
    let listener = Listener::start("lab").unwrap();
    let (said, progress) = mpsc::channel();
    let (noticed, heard) = mpsc::channel();
    let first = std::thread::spawn({
        let (channel, listener) = (helper.channel.clone(), listener.clone());
        move || channel.start_runtime(&listener, &StartOptions::default(), &mut |message| drop(said.send(message)), move |notice| drop(noticed.send(notice)))
    });
    let [one] = helper.starts_sent(1)[..] else { panic!() };
    let began = Instant::now();
    let second = helper.channel.start_runtime(&listener, &StartOptions::default(), &mut |_| {}, |_| {});
    assert_eq!(second.unwrap_err(), StartError::Failed("Julia is already starting.".into()));
    assert!(began.elapsed() < Duration::from_millis(500));
    assert_eq!(helper.requests().iter().filter(|request| matches!(request, ToHelper::StartRuntime { .. })).count(), 1, "nothing was sent for it");

    helper.tell(&ToApp::Progress { line: "installing".into() });
    helper.tell(&ready(one));
    first.join().unwrap().expect("the first start is ready");
    assert!(matches!(progress.recv_timeout(Duration::from_secs(5)), Ok(ToApp::Progress { line }) if line == "installing"));
    helper.tell(&ToApp::Died { status: "exit status: 1".into(), log_tail: Vec::new() });
    let notice = heard.recv_timeout(Duration::from_secs(5)).expect("the first runtime is still watched");
    assert!(matches!(notice, Notice::Died(_)), "{notice:?}");
}

#[test]
fn a_stop_on_a_channel_whose_helper_has_gone_fails_at_once() {
    let helper = Scripted::new();
    let Scripted { channel, say, .. } = helper;
    drop(say);
    channel.closed();
    let began = Instant::now();
    assert_eq!(channel.stop().unwrap_err(), CLOSED);
    assert!(began.elapsed() < Duration::from_millis(500), "{:?}", began.elapsed());
}

#[test]
fn a_stop_that_gave_up_does_not_undo_a_quit_made_meanwhile() {
    let helper = Scripted::new();
    let (noticed, heard) = mpsc::channel();
    helper.started(&Listener::start("lab").unwrap(), move |notice| drop(noticed.send(notice)));
    let stopping = helper.stopping();
    helper.stops_sent(1);
    helper.channel.quit(true);
    assert!(stopping.join().unwrap().unwrap_err().contains("didn't answer"));
    assert!(helper.channel.waiting.lock().unwrap().is_empty(), "nothing waits for the stop's answer");
    helper.tell(&ToApp::Died { status: "exit status: 1".into(), log_tail: Vec::new() });
    assert!(heard.recv_timeout(Duration::from_millis(500)).is_err(), "the client left, so its watcher stays quiet");
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
    // The helper's output stays open, so only the drop could end its input.
    let Scripted { channel, say, closed, .. } = helper;
    drop(channel);
    assert!(!closed.load(Ordering::SeqCst));
    drop(say);
}

#[test]
fn a_stop_the_helper_refuses_is_an_error_and_the_runtime_stays_watched() {
    let helper = Scripted::new();
    let (noticed, heard) = mpsc::channel();
    helper.started(&Listener::start("lab").unwrap(), move |notice| drop(noticed.send(notice)));
    let stopping = helper.stopping();
    let [stop] = helper.stops_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::NotStopped { id: stop, message: "Julia was not stopped: busy.".into() });
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
    let stopping = helper.stopping();
    let [stop] = helper.stops_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::Stopped { id: stop });
    stopping.join().unwrap().unwrap();
    helper.tell(&ToApp::Died { status: "exit status: 1".into(), log_tail: Vec::new() });
    assert!(heard.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn two_stops_at_once_are_each_answered_by_id_and_leave_nothing_for_the_next_start() {
    let helper = Scripted::new();
    let (first, second) = (helper.stopping(), helper.stopping());
    let ids = helper.stops_sent(2);
    // The second to be sent is answered first, and the other one is refused.
    helper.tell(&ToApp::NotStopped { id: ids[1], message: "no lock".into() });
    helper.tell(&ToApp::Stopped { id: ids[0] });
    let (first, second) = (first.join().unwrap(), second.join().unwrap());
    assert_eq!(first.is_ok() as u8 + second.is_ok() as u8, 1, "{first:?} {second:?}");
    assert_eq!(first.err().or(second.err()), Some("no lock".into()));
    let began = Instant::now();
    helper.started(&Listener::start("lab").unwrap(), |_| {});
    assert!(began.elapsed() < Duration::from_millis(900), "no answer was left over to fail the start");
}

#[test]
fn a_stop_that_was_refused_does_not_hold_up_the_next_one() {
    let helper = Scripted::new();
    let began = Instant::now();
    let first = helper.stopping();
    let [one] = helper.stops_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::NotStopped { id: one, message: "no lock".into() });
    assert_eq!(first.join().unwrap(), Err("no lock".into()));
    let second = helper.stopping();
    let [_, two] = helper.stops_sent(2)[..] else { panic!() };
    helper.tell(&ToApp::Stopped { id: two });
    second.join().unwrap().unwrap();
    assert!(began.elapsed() < Duration::from_millis(900), "{:?}", began.elapsed());
    helper.started(&Listener::start("lab").unwrap(), |_| {});
}

#[test]
fn a_quit_that_stops_ends_a_start_under_way() {
    let helper = Scripted::new();
    let starting = helper.starting(&Listener::start("lab").unwrap(), |_| {});
    let [start] = helper.starts_sent(1)[..] else { panic!() };
    helper.channel.quit(false);
    let [stop] = helper.stops_sent(1)[..] else { panic!() };
    assert_ne!(start, stop);
    helper.tell(&ToApp::StartCancelled { id: start });
    helper.tell(&ToApp::Stopped { id: stop });
    assert_eq!(starting.join().unwrap().unwrap_err(), "Julia was stopped while it started.");
}

#[test]
fn a_stop_from_another_thread_ends_a_start_under_way() {
    let helper = Scripted::new();
    let starting = helper.starting(&Listener::start("lab").unwrap(), |_| {});
    let [start] = helper.starts_sent(1)[..] else { panic!() };
    let stopping = helper.stopping();
    let [stop] = helper.stops_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::StartCancelled { id: start });
    helper.tell(&ToApp::Stopped { id: stop });
    assert_eq!(starting.join().unwrap().unwrap_err(), "Julia was stopped while it started.");
    stopping.join().unwrap().unwrap();
}

#[test]
fn two_stops_sent_from_two_threads_at_once_are_each_answered() {
    let helper = Scripted::new();
    let threads: Vec<_> = (0..8).map(|_| helper.stopping()).collect();
    let mut ids = helper.stops_sent(8);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 8, "each has an id of its own");
    for id in ids.into_iter().rev() {
        helper.tell(&ToApp::Stopped { id });
    }
    for thread in threads {
        thread.join().unwrap().unwrap();
    }
}

#[test]
fn a_refusal_lets_the_runtime_be_watched_again_even_when_its_death_follows_at_once() {
    let helper = Scripted::new();
    let (noticed, heard) = mpsc::channel();
    helper.started(&Listener::start("lab").unwrap(), move |notice| drop(noticed.send(notice)));
    let stopping = helper.stopping();
    let [stop] = helper.stops_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::NotStopped { id: stop, message: "busy".into() });
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
    let [stop] = helper.stops_sent(1)[..] else { panic!() };
    helper.tell(&ToApp::NotStopped { id: stop, message: "busy".into() });
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
