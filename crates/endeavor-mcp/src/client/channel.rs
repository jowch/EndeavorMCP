//! The client's end of `endeavor connect`'s stdin and stdout, on this computer
//! or over ssh: the helper's hello, file requests, and a runtime starting,
//! dying and stopping on it. It lasts as long as the helper does.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use wire::files;
use wire::relay::Mux;
use wire::slurm::JobRequest;
use wire::{Frame, ToApp, ToHelper};

use super::listener::Listener;

/// How long `stop` waits for the helper's answer: the helper waits up to 20 s
/// for the start lock, then stopping takes up to 20 s more.
const STOP_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 60 });

/// A runtime the client is attached to, as its server's listener serves it.
#[derive(Clone, Debug)]
pub struct Runtime {
    /// The listener's port on this computer; every request goes through it with the token.
    pub port: u16,
    /// The runtime's token, for the `Authorization: Bearer` header.
    pub token: String,
    /// The MCP URL on the listener, the same for the listener's whole life.
    pub mcp_url: String,
    /// Pluto's start page, with the token that lets a browser in.
    pub page_url: String,
    /// Its process, which tells it from the server's next runtime (the URLs
    /// and the token stay the same).
    pub pid: u32,
    /// It was already running; this connect didn't start it.
    pub reattached: bool,
    /// The machine it runs on.
    pub node: String,
    /// The cluster job it runs in.
    pub job: Option<wire::slurm::Job>,
}

/// Why a runtime went away.
#[derive(Debug)]
pub enum Notice {
    /// Julia exited; the helper is still connected, so it can start again.
    Died(String),
    /// Another client took the runtime over.
    Replaced,
    /// The helper failed or the connection to it closed.
    Lost(String),
}

/// Why `start_runtime_with` didn't give a runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum StartError {
    /// No Julia was found there, and a download wasn't allowed: nothing was
    /// started or downloaded. What a download would be.
    NoJulia(String),
    Failed(String),
}

impl StartError {
    /// The words for an error, whichever it is.
    pub fn message(self) -> String {
        match self {
            StartError::NoJulia(offer) => format!("Julia wasn't found on that machine. {offer}"),
            StartError::Failed(message) => message,
        }
    }
}

/// What the helper says as soon as it runs.
#[derive(Clone, Debug)]
pub struct Hello {
    pub node: String,
    /// The home folder on its machine.
    pub home: PathBuf,
    /// Slurm's commands are there: probably a cluster's login node.
    pub slurm: bool,
    /// It can save attached files into a session's folder (`files::Request::Write`).
    pub uploads: bool,
}

/// Who is owed the helper's answer to a `Stop`.
enum Owed {
    /// A `stop` that is waiting for it.
    To(mpsc::Sender<ToApp>),
    /// A `stop` that gave up waiting, or a quit that waits for none; the answer is dropped.
    Nobody,
}

/// A `Stop` that was sent and isn't answered yet.
struct Pending {
    number: u64,
    owed: Owed,
    /// The runtime's flag that keeps its watcher quiet, which the answer
    /// `NotStopped` lowers: the runtime goes on, and so does its watcher. A
    /// quit has none, since the client is leaving anyway.
    leaving: Option<Arc<AtomicBool>>,
}

/// The `Stop`s sent whose answer hasn't come, and where a start stands.
#[derive(Default)]
struct Stops {
    /// Oldest first. The helper answers each `Stop` with one `Stopped` or
    /// `NotStopped`, in the order it got them, so an answer belongs to the
    /// first entry.
    queue: VecDeque<Pending>,
    /// The number the next `Stop` had when the start under way began. A
    /// `Stopped` that answers a `Stop` numbered from it on also ends that
    /// start: the helper has one `Stopped` for both.
    starting: Option<u64>,
}

/// What a start or a stop says when the helper's connection ended under it.
pub const CLOSED: &str = "The connection to Endeavor's helper closed.";

/// The helper's input behind its lock; taking it out closes the helper's input.
type SharedInput = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// The helper's input, which the channel closes to let the helper see the end of it.
struct Input(SharedInput);

impl Write for Input {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.flush()
    }
}

/// The client's end of a helper's stdin/stdout, on this computer or over ssh. It
/// lasts as long as the helper: runtimes start, die and stop on it.
pub struct Channel {
    mux: Arc<Mux>,
    input: SharedInput,
    /// Where control messages other than file replies go: to whoever waits on
    /// the helper now (its hello, a start, a stop, or the runtime's watcher).
    /// Replacing it ends the previous listener's wait.
    sink: Arc<Mutex<Option<mpsc::Sender<ToApp>>>>,
    hello: Mutex<Option<mpsc::Receiver<ToApp>>>,
    files: Arc<Mutex<HashMap<u32, mpsc::Sender<files::Reply>>>>,
    next_file: AtomicU32,
    /// Set once the helper has exited (or its end of the channel closed).
    ended: Arc<(Mutex<bool>, Condvar)>,
    /// The client let the helper go (a detach, a quit or dropping the channel), so its end is no drop.
    left: Arc<AtomicBool>,
    /// The listener relaying to this channel, which forgets it when it ends.
    listener: Arc<Mutex<Option<Arc<Listener>>>>,
    /// Set when the client stops the current runtime or leaves the helper, so its
    /// watcher keeps quiet.
    leaving: Mutex<Arc<AtomicBool>>,
    /// An answer never reads as the outcome of the next `start_runtime`: a
    /// `stop` that gave up leaves its entry as `Nobody`. The reader hands
    /// answers out, and `stop` gives up, under this lock.
    stops: Arc<Mutex<Stops>>,
    next_stop: AtomicU64,
    /// Held while a `Stop` or a `StartRuntime` is queued and sent, so the
    /// helper gets them in the order the queue has them. The reader never
    /// takes it, so a send that blocks on a full pipe can't stop the reader.
    sending: Mutex<()>,
}

impl Channel {
    /// `output` is the helper's stdout, read up to its first frame.
    pub fn open(helper: Child, input: impl Write + Send + 'static, output: impl Read + Send + 'static) -> Channel {
        Channel::open_watched(helper, input, output, || {})
    }

    /// `open`, and `exited` is called once the helper has exited and before it
    /// is reaped, so its pid is still its own.
    pub fn open_watched(mut helper: Child, input: impl Write + Send + 'static, output: impl Read + Send + 'static, exited: impl FnOnce() + Send + 'static) -> Channel {
        let input: SharedInput = Arc::new(Mutex::new(Some(Box::new(input))));
        let mux = Mux::new(Input(input.clone()));
        let (hello_tx, hello) = mpsc::channel::<ToApp>();
        let sink = Arc::new(Mutex::new(Some(hello_tx)));
        let ended: Arc<(Mutex<bool>, Condvar)> = Arc::default();
        let listener: Arc<Mutex<Option<Arc<Listener>>>> = Arc::default();
        let files: Arc<Mutex<HashMap<u32, mpsc::Sender<files::Reply>>>> = Arc::default();
        let stops: Arc<Mutex<Stops>> = Arc::default();
        let left = Arc::new(AtomicBool::new(false));
        std::thread::spawn({
            let (mux, sink, listener, files, ended, stops, left) = (mux.clone(), sink.clone(), listener.clone(), files.clone(), ended.clone(), stops.clone(), left.clone());
            move || {
                let result = mux.run(
                    output,
                    // The client opens every stream; the helper never asks to.
                    |mux, id| drop(mux.send(&Frame::Close { id })),
                    |json| match serde_json::from_slice(json) {
                        Ok(ToApp::Files { id, reply }) => {
                            if let Some(waiting) = files.lock().unwrap().remove(&id) {
                                let _ = waiting.send(reply);
                            }
                        }
                        Ok(message) => {
                            let mut stops = stops.lock().unwrap();
                            let answers_stop = matches!(message, ToApp::Stopped | ToApp::NotStopped { .. });
                            let to_sink = |message| {
                                if let Some(sink) = &*sink.lock().unwrap() {
                                    let _ = sink.send(message);
                                }
                            };
                            let Some(Pending { number, owed, leaving }) = answers_stop.then(|| stops.queue.pop_front()).flatten() else {
                                return to_sink(message);
                            };
                            if matches!(message, ToApp::NotStopped { .. })
                                && let Some(leaving) = leaving
                            {
                                leaving.store(false, Ordering::SeqCst);
                            }
                            if matches!(message, ToApp::Stopped) && stops.starting.is_some_and(|began| number >= began) {
                                to_sink(ToApp::Stopped);
                            }
                            if let Owed::To(waiting) = owed {
                                let _ = waiting.send(message);
                            }
                        }
                        Err(e) => eprintln!("The runtime helper sent an unreadable message: {e}"),
                    },
                );
                if let Some(listener) = listener.lock().unwrap().take() {
                    if left.load(Ordering::SeqCst) {
                        listener.left(&mux);
                    } else {
                        listener.forget(&mux);
                    }
                }
                // Whoever waits hears the end.
                sink.lock().unwrap().take();
                files.lock().unwrap().clear();
                *stops.lock().unwrap() = Stops::default();
                exit_unreaped(&mut helper);
                exited();
                let status = helper.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                eprintln!("The runtime helper exited ({status}){}", result.err().map(|e| format!(": {e}")).unwrap_or_default());
                *ended.0.lock().unwrap() = true;
                ended.1.notify_all();
            }
        });
        Channel {
            mux,
            input,
            sink,
            hello: Mutex::new(Some(hello)),
            files,
            next_file: AtomicU32::new(0),
            ended,
            left,
            listener,
            leaving: Mutex::default(),
            stops,
            next_stop: AtomicU64::new(0),
            sending: Mutex::new(()),
        }
    }

    /// Control messages from now on, to the returned receiver only.
    fn subscribe(&self) -> mpsc::Receiver<ToApp> {
        let (tx, rx) = mpsc::channel();
        let mut sink = self.sink.lock().unwrap();
        // A channel whose helper is gone keeps no sink, so the receiver ends at once.
        if sink.is_some() {
            *sink = Some(tx);
        }
        rx
    }

    /// Wait for the helper's hello; `vanished` says why when the helper just ends.
    pub fn wait_hello(&self, vanished: impl FnOnce() -> String) -> Result<Hello, String> {
        let Some(hello) = self.hello.lock().unwrap().take() else { return Err("Already said hello.".into()) };
        match hello.recv() {
            Ok(ToApp::Hello { node, home, slurm, uploads, .. }) => Ok(Hello { node, home: PathBuf::from(home), slurm, uploads }),
            Ok(ToApp::Error { message }) => Err(message),
            Ok(other) => Err(format!("Endeavor's helper said {other:?} before hello.")),
            Err(_) => Err(vanished()),
        }
    }

    /// Ask the helper about its machine's files.
    pub fn files(&self, request: files::Request) -> Result<files::Reply, String> {
        let id = self.next_file.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.files.lock().unwrap().insert(id, tx);
        self.mux.send(&ToHelper::Files { id, request }.frame()).map_err(|_| "The connection closed.".to_owned())?;
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(files::Reply::Error { message }) => Err(message),
            Ok(reply) => Ok(reply),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.files.lock().unwrap().remove(&id);
                Err("The server took too long to answer.".into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("The connection closed.".into()),
        }
    }

    /// Start the runtime (or attach to the running one) and relay `listener`'s
    /// connections to it from now on; on a cluster, `job` is what to submit.
    /// Blocks until it's ready; `on_message` hears `Progress`, `FoundJulia`,
    /// `Submitted` and `Queued` meanwhile, and `notice` the first word of the
    /// runtime going away later, unless the client is the one stopping it.
    /// The helper may download Julia if it finds none.
    pub fn start_runtime(&self, listener: &Arc<Listener>, job: Option<JobRequest>, on_message: &mut dyn FnMut(ToApp), notice: impl FnOnce(Notice) + Send + 'static) -> Result<Runtime, String> {
        self.start_runtime_with(listener, job, true, on_message, notice).map_err(StartError::message)
    }

    /// `start_runtime`, and `download_julia` false has the helper download
    /// nothing: it ends with `StartError::NoJulia` when it finds no Julia.
    /// That is sent only to a helper of this build, which knows it (`ToHelper::StartRuntime`).
    pub fn start_runtime_with(
        &self,
        listener: &Arc<Listener>,
        job: Option<JobRequest>,
        download_julia: bool,
        on_message: &mut dyn FnMut(ToApp),
        notice: impl FnOnce(Notice) + Send + 'static,
    ) -> Result<Runtime, StartError> {
        let failed = StartError::Failed;
        let events = self.subscribe();
        let leaving = Arc::new(AtomicBool::new(false));
        *self.leaving.lock().unwrap() = leaving.clone();
        let _starting = Starting(&self.stops);
        {
            let _sending = self.sending.lock().unwrap();
            self.stops.lock().unwrap().starting = Some(self.next_stop.load(Ordering::SeqCst));
            self.mux.send(&ToHelper::StartRuntime { job, download_julia }.frame()).map_err(|_| failed(CLOSED.to_owned()))?;
        }
        let runtime = loop {
            match events.recv() {
                Ok(message @ (ToApp::Progress { .. } | ToApp::FoundJulia { .. } | ToApp::Submitted { .. } | ToApp::Queued { .. })) => on_message(message),
                Ok(ToApp::Ready { node, pid, token, reattached, job, .. }) => {
                    *self.listener.lock().unwrap() = Some(listener.clone());
                    listener.attach(self.mux.clone(), token.clone());
                    break Runtime { port: listener.port(), mcp_url: listener.mcp_url(), page_url: listener.page_url(&token), token, pid, reattached, node, job };
                }
                Ok(ToApp::StartFailed { message } | ToApp::Error { message }) => return Err(failed(message)),
                Ok(ToApp::NoJulia { offer }) => return Err(StartError::NoJulia(offer)),
                Ok(ToApp::Died { status, log_tail }) => {
                    let how = died_reason(&status, &[]);
                    return Err(failed(format!("Julia stopped before Pluto was ready. {how}{}{}", if how.is_empty() { "" } else { " " }, diagnose(&log_tail))));
                }
                Ok(ToApp::Stopped) => return Err(failed("Julia was stopped while it started.".into())),
                Ok(ToApp::Replaced) => return Err(failed("Another connection took Julia over while it was starting.".into())),
                Ok(ToApp::Hello { .. } | ToApp::Files { .. } | ToApp::NotStopped { .. }) => {}
                Err(_) => return Err(failed(CLOSED.into())),
            }
        };
        std::thread::spawn(move || {
            let heard = loop {
                match events.recv() {
                    Ok(ToApp::Died { status, log_tail }) => break Notice::Died(died_reason(&status, &log_tail)),
                    Ok(ToApp::Replaced) => break Notice::Replaced,
                    Ok(ToApp::Error { message }) => break Notice::Lost(message),
                    Ok(_) => continue,
                    // The helper ended: `closed` tells of that.
                    Err(_) => return,
                }
            };
            if !leaving.load(Ordering::SeqCst) {
                notice(heard);
            }
        });
        Ok(runtime)
    }

    fn leave(&self) {
        self.leaving.lock().unwrap().store(true, Ordering::SeqCst);
    }

    /// Queue a `Stop` and send it, in one step so that the helper gets `Stop`s
    /// (and a `StartRuntime`) in the order they are queued. Its number.
    fn send_stop(&self, owed: Owed, leaving: Option<Arc<AtomicBool>>) -> Result<u64, ()> {
        let _sending = self.sending.lock().unwrap();
        let number = self.next_stop.fetch_add(1, Ordering::SeqCst);
        self.stops.lock().unwrap().queue.push_back(Pending { number, owed, leaving });
        if self.mux.send(&ToHelper::Stop.frame()).is_err() {
            self.stops.lock().unwrap().queue.retain(|pending| pending.number != number);
            return Err(());
        }
        Ok(number)
    }

    /// Stop the runtime and wait until it's gone (blocks up to ~60 s). The
    /// helper stays connected. If the helper says it didn't stop, the runtime is
    /// still attached and is watched as before. A start under way on another
    /// thread ends, since the helper's `Stopped` ends it too.
    pub fn stop(&self) -> Result<(), String> {
        let leaving = self.leaving.lock().unwrap().clone();
        leaving.store(true, Ordering::SeqCst);
        let (tx, answer) = mpsc::channel();
        let Ok(id) = self.send_stop(Owed::To(tx), Some(leaving)) else {
            return Err(CLOSED.into());
        };
        let answered = match answer.recv_timeout(STOP_WAIT) {
            Ok(message) => Ok(message),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(CLOSED.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut stops = self.stops.lock().unwrap();
                // The reader answers under this lock, so an answer that came as the wait ended is here.
                answer.try_recv().map_err(|_| {
                    if let Some(pending) = stops.queue.iter_mut().find(|pending| pending.number == id) {
                        pending.owed = Owed::Nobody;
                    }
                })
            }
        };
        match answered {
            Ok(ToApp::NotStopped { message }) => Err(message),
            Ok(_) => Ok(()),
            Err(()) => Err(format!("Endeavor's helper didn't answer in {} s, so Julia may not have stopped.", STOP_WAIT.as_secs())),
        }
    }

    /// Leave the runtime running and wait until the helper has gone.
    pub fn detach(&self) {
        self.leave();
        self.left.store(true, Ordering::SeqCst);
        let _ = self.mux.send(&ToHelper::Detach.frame());
        self.wait_end(Some(Duration::from_secs(30)));
    }

    /// Block until the helper has gone, or `timeout` passes.
    fn wait_end(&self, timeout: Option<Duration>) {
        let (ended, changed) = &*self.ended;
        let ended = ended.lock().unwrap();
        match timeout {
            Some(timeout) => drop(changed.wait_timeout_while(ended, timeout, |ended| !*ended)),
            None => drop(changed.wait_while(ended, |ended| !*ended)),
        }
    }

    /// The helper has gone, by itself or because the client let it go.
    pub fn is_closed(&self) -> bool {
        *self.ended.0.lock().unwrap()
    }

    /// Block until the helper has gone, whether or not Julia runs: `Lost` if
    /// it went by itself (ssh or the helper exited, or the network dropped and
    /// ssh's keepalive gave up), None if the client let it go.
    pub fn closed(&self) -> Option<Notice> {
        self.wait_end(None);
        (!self.left.load(Ordering::SeqCst)).then(|| Notice::Lost("The connection closed unexpectedly.".into()))
    }

    /// The client is quitting: leave Julia running, or stop it. The helper does
    /// the rest after the client has gone.
    pub fn quit(&self, keep_running: bool) {
        self.leave();
        self.left.store(true, Ordering::SeqCst);
        if keep_running {
            let _ = self.mux.send(&ToHelper::Detach.frame());
            return;
        }
        // Its answer ends a start that may be under way.
        let _ = self.send_stop(Owed::Nobody, None);
    }
}

/// While it lives, a start is under way (`Stops::starting`).
struct Starting<'a>(&'a Mutex<Stops>);

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap().starting = None;
    }
}

impl Drop for Channel {
    /// A channel nobody holds any more ends the helper's input, as a client that
    /// vanished does, so the helper applies its own rule (`--quit-with-client`)
    /// and ssh and the helper don't outlive a failed connect or a panic.
    fn drop(&mut self) {
        if !self.left.swap(true, Ordering::SeqCst) {
            self.leave();
            self.input.lock().unwrap().take();
        }
    }
}

/// Wait until `child` has exited without reaping it, so its pid is not given to
/// another process yet.
#[cfg(unix)]
fn exit_unreaped(child: &mut Child) {
    loop {
        // SAFETY: waitid fills in the zeroed siginfo_t it is given; WNOWAIT leaves the child to `wait`.
        let done = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if done == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// A process id stays its own while a handle to the process is open, which `child` holds.
#[cfg(windows)]
fn exit_unreaped(child: &mut Child) {
    let _ = child.wait();
}

/// Plain-language cause for common failures in Julia's log, if recognized.
fn hint(log: &str) -> Option<&'static str> {
    if ["Could not resolve host", "failed to clone", "Couldn't connect", "network"]
        .iter()
        .any(|p| log.contains(p))
    {
        Some("It couldn't download packages; check the internet connection (the first launch installs Pluto and its packages).")
    } else if log.contains("Unsatisfiable requirements") {
        Some("Package versions in Endeavor's runtime environment conflict.")
    } else if log.contains("EADDRINUSE") || log.contains("Address already in use") {
        Some("A port it needs is already in use.")
    } else {
        None
    }
}

/// Why Julia stopped, in plain words, for "Julia on <host> stopped.": how a
/// cluster job ended (the helper's sentence), else how the process exited,
/// plus a cause from its log if one is recognized. A crash's log is mostly
/// Pluto's startup banner, so only a recognized cause is shown. Empty when
/// nothing is known.
pub fn died_reason(status: &str, log_tail: &[String]) -> String {
    let how = if status.ends_with('.') {
        Some(status.to_owned())
    } else if let Some(code) = status.strip_prefix("exit status: ") {
        Some(format!("It exited with code {code}."))
    } else if let Some(signal) = status.strip_prefix("signal: ") {
        let memory = if signal.starts_with("9 ") { ", perhaps for using too much memory" } else { "" };
        Some(format!("It was killed (signal {signal}){memory}."))
    } else {
        None
    };
    how.into_iter().chain(hint(&log_tail.join("\n")).map(str::to_owned)).collect::<Vec<_>>().join(" ")
}

/// Plain-language cause for common failures, else the end of Julia's log.
fn diagnose(tail: &[String]) -> String {
    let hint = hint(&tail.join("\n"));
    let recent: Vec<&str> = tail.iter().rev().take(12).rev().map(String::as_str).collect();
    match hint {
        Some(hint) => hint.to_string(),
        None if recent.is_empty() => "It printed nothing.".to_string(),
        None => format!("Last output:\n{}", recent.join("\n")),
    }
}

#[cfg(test)]
mod tests;
