use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use wire::files;
use wire::relay::Mux;
use wire::slurm::JobRequest;
use wire::{Frame, ToApp, ToHelper};

use super::listener::Listener;
use super::machines::Launcher;

const STOP_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 60 });

#[derive(Clone, Debug)]
pub struct Runtime {
    pub port: u16,
    pub token: String,
    pub page_token: String,
    pub mcp_url: String,
    pub page_url: String,
    pub pid: u32,
    pub reattached: bool,
    pub node: String,
    pub job: Option<wire::slurm::Job>,
    pub remote_port: Option<u16>,
    pub build: Option<String>,
    pub interface: Option<u32>,
}

#[derive(Debug)]
pub enum Notice {
    Died(String),
    Lost(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StartOptions {
    pub job: Option<JobRequest>,
    pub engine: String,
    pub install: bool,
    pub attach_only: bool,
}

impl Default for StartOptions {
    fn default() -> StartOptions {
        StartOptions { job: None, engine: wire::ENGINE_PLUTO.to_owned(), install: false, attach_only: false }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StartError {
    NeedsInstall(Vec<wire::Item>),
    NotRunning,
    Failed(String),
}

impl StartError {
    pub fn message(self) -> String {
        match self {
            StartError::NeedsInstall(items) => format!("{} Installing wasn't allowed, and an agreement to it (`StartOptions::install`) is for one start only.", wire::needs_text(&items, "that machine")),
            StartError::NotRunning => "Endeavor isn't running there.".to_owned(),
            StartError::Failed(message) => message,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hello {
    pub protocol: u32,
    pub node: String,
    pub home: String,
    pub slurm: bool,
    pub uploads: bool,
    pub launcher: Option<Launcher>,
}

impl Hello {
    pub fn other_version(&self) -> Option<String> {
        (self.protocol != wire::PROTOCOL).then(|| {
            format!(
                "The server side of this connection is of another version of Endeavor (its protocol is {}, this one's is {}). Use the same version of Endeavor on both sides.",
                self.protocol,
                wire::PROTOCOL
            )
        })
    }
}

struct Waiter {
    to: mpsc::Sender<ToApp>,
    leaving: Option<Arc<AtomicU32>>,
}

type Waiting = Arc<Mutex<HashMap<u32, Waiter>>>;

struct Registered {
    waiting: Waiting,
    id: u32,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.waiting.lock().unwrap().remove(&self.id);
    }
}

struct Starting<'a>(&'a AtomicBool);

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub const CLOSED: &str = "The connection to Endeavor's helper closed.";

type SharedInput = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

struct Input(SharedInput);

impl Write for Input {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.flush()
    }
}

pub struct Channel {
    mux: Arc<Mux>,
    input: SharedInput,
    sink: Arc<Mutex<Option<mpsc::Sender<ToApp>>>>,
    hello: Mutex<Option<mpsc::Receiver<ToApp>>>,
    files: Arc<Mutex<HashMap<u32, mpsc::Sender<files::Reply>>>>,
    next_file: AtomicU32,
    ended: Arc<(Mutex<bool>, Condvar)>,
    left: Arc<AtomicBool>,
    listener: Arc<Mutex<Option<Arc<Listener>>>>,
    leaving: Mutex<Arc<AtomicU32>>,
    starting: AtomicBool,
    waiting: Waiting,
    next_id: AtomicU32,
}

impl Channel {
    pub fn open(helper: Child, input: impl Write + Send + 'static, output: impl Read + Send + 'static) -> Channel {
        Channel::open_watched(helper, input, output, || {})
    }

    pub fn open_watched(mut helper: Child, input: impl Write + Send + 'static, output: impl Read + Send + 'static, exited: impl FnOnce() + Send + 'static) -> Channel {
        let input: SharedInput = Arc::new(Mutex::new(Some(Box::new(input))));
        let mux = Mux::new(Input(input.clone()));
        let (hello_tx, hello) = mpsc::channel::<ToApp>();
        let sink = Arc::new(Mutex::new(Some(hello_tx)));
        let ended: Arc<(Mutex<bool>, Condvar)> = Arc::default();
        let listener: Arc<Mutex<Option<Arc<Listener>>>> = Arc::default();
        let files: Arc<Mutex<HashMap<u32, mpsc::Sender<files::Reply>>>> = Arc::default();
        let waiting: Waiting = Arc::default();
        let left = Arc::new(AtomicBool::new(false));
        std::thread::spawn({
            let (mux, sink, listener, files, ended, waiting, left) = (mux.clone(), sink.clone(), listener.clone(), files.clone(), ended.clone(), waiting.clone(), left.clone());
            move || {
                let result = mux.run(
                    output,
                    |mux, id| drop(mux.send(&Frame::Close { id })),
                    |json| match serde_json::from_slice(json) {
                        Ok(ToApp::Files { id, reply }) => {
                            if let Some(waiting) = files.lock().unwrap().remove(&id) {
                                let _ = waiting.send(reply);
                            }
                        }
                        Ok(message) => match message.answers() {
                            Some(id) => {
                                let mut waiting = waiting.lock().unwrap();
                                if let Some(Waiter { to, leaving }) = waiting.remove(&id) {
                                    if matches!(message, ToApp::NotStopped { .. })
                                        && let Some(leaving) = leaving
                                    {
                                        leaving.fetch_sub(1, Ordering::SeqCst);
                                    }
                                    let _ = to.send(message);
                                }
                            }
                            None => {
                                if let Some(sink) = &*sink.lock().unwrap() {
                                    let _ = sink.send(message);
                                }
                            }
                        },
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
                sink.lock().unwrap().take();
                files.lock().unwrap().clear();
                waiting.lock().unwrap().clear();
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
            starting: AtomicBool::new(false),
            waiting,
            next_id: AtomicU32::new(1),
        }
    }

    fn register(&self, id: u32, leaving: Option<Arc<AtomicU32>>, progress: bool) -> (mpsc::Receiver<ToApp>, Registered) {
        let (tx, rx) = mpsc::channel();
        let mut sink = self.sink.lock().unwrap();
        if let Some(sink) = sink.as_mut() {
            if progress {
                *sink = tx.clone();
            }
            self.waiting.lock().unwrap().insert(id, Waiter { to: tx, leaving });
        }
        (rx, Registered { waiting: self.waiting.clone(), id })
    }

    fn request_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn wait_hello(&self, vanished: impl FnOnce() -> String) -> Result<Hello, String> {
        let Some(hello) = self.hello.lock().unwrap().take() else { return Err("Already said hello.".into()) };
        match hello.recv() {
            Ok(ToApp::Hello { protocol, node, home, slurm, uploads, launcher, .. }) => {
                let launcher = match launcher.as_str() {
                    "process" => Some(Launcher::Process),
                    "slurm" => Some(Launcher::Slurm),
                    _ => None,
                };
                Ok(Hello { protocol, node, home, slurm, uploads, launcher })
            }
            Ok(ToApp::Error { message }) => Err(message),
            Ok(other) => Err(format!("Endeavor's helper said {other:?} before hello.")),
            Err(_) => Err(vanished()),
        }
    }

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

    pub fn start_runtime(&self, listener: &Arc<Listener>, options: &StartOptions, on_message: &mut dyn FnMut(ToApp), notice: impl FnOnce(Notice) + Send + 'static) -> Result<Runtime, StartError> {
        let failed = StartError::Failed;
        if self.starting.swap(true, Ordering::SeqCst) {
            return Err(failed("Endeavor is already starting.".into()));
        }
        let _starting = Starting(&self.starting);
        let id = self.request_id();
        let (events, _registered) = self.register(id, None, true);
        let leaving = Arc::new(AtomicU32::new(0));
        *self.leaving.lock().unwrap() = leaving.clone();
        self.mux.send(&ToHelper::StartRuntime { id, job: options.job.clone(), engine: options.engine.clone(), install: options.install, attach_only: options.attach_only }.frame()).map_err(|_| failed(CLOSED.to_owned()))?;
        let runtime = loop {
            match events.recv() {
                Ok(message @ (ToApp::Progress { .. } | ToApp::Found { .. } | ToApp::Submitted { .. } | ToApp::Queued { .. })) => on_message(message),
                Ok(ToApp::Ready { node, pid, token, page_token, reattached, job, port, build, interface, .. }) => {
                    let page_token = page_token.unwrap_or_else(|| token.clone());
                    *self.listener.lock().unwrap() = Some(listener.clone());
                    listener.attach(self.mux.clone(), token.clone(), build.is_some() || interface.is_some(), (!crate::usable_as_is(build.as_deref(), interface)).then_some(interface));
                    if self.mux.has_ended() {
                        if self.left.load(Ordering::SeqCst) {
                            listener.left(&self.mux);
                        } else {
                            listener.forget(&self.mux);
                        }
                    }
                    break Runtime { port: listener.port(), mcp_url: listener.mcp_url(), page_url: listener.page_url(&page_token), page_token, token, pid, reattached, node, job, remote_port: port, build, interface };
                }
                Ok(ToApp::StartFailed { message, .. } | ToApp::Error { message }) => return Err(failed(message)),
                Ok(ToApp::NeedsInstall { items, .. }) if items.is_empty() => return Err(failed("The helper says the start needs something installed, but not what.".into())),
                Ok(ToApp::NeedsInstall { items, .. }) => return Err(StartError::NeedsInstall(items)),
                Ok(ToApp::StartDied { status, log_tail, .. }) => {
                    let how = died_reason(&status, &[]);
                    return Err(failed(format!("Endeavor stopped before it was ready. {how}{}{}", if how.is_empty() { "" } else { " " }, diagnose(&log_tail))));
                }
                Ok(ToApp::StartCancelled { .. }) => return Err(failed("Endeavor was stopped while it started.".into())),
                Ok(ToApp::NotRunning { .. }) => return Err(StartError::NotRunning),
                Ok(_) => {}
                Err(_) => return Err(failed(CLOSED.into())),
            }
        };
        std::thread::spawn(move || {
            let heard = loop {
                match events.recv() {
                    Ok(ToApp::Died { status, log_tail }) => break Notice::Died(died_reason(&status, &log_tail)),
                    Ok(ToApp::Error { message }) => break Notice::Lost(message),
                    Ok(_) => continue,
                    Err(_) => return,
                }
            };
            if leaving.load(Ordering::SeqCst) == 0 {
                notice(heard);
            }
        });
        Ok(runtime)
    }

    fn leave(&self) {
        self.leaving.lock().unwrap().fetch_add(1, Ordering::SeqCst);
    }

    pub fn stop(&self) -> Result<(), String> {
        self.stop_as(false)
    }

    pub fn force_stop(&self) -> Result<(), String> {
        self.stop_as(true)
    }

    fn stop_as(&self, force: bool) -> Result<(), String> {
        let leaving = self.leaving.lock().unwrap().clone();
        leaving.fetch_add(1, Ordering::SeqCst);
        let id = self.request_id();
        let (answer, _registered) = self.register(id, Some(leaving.clone()), false);
        if self.mux.send(&ToHelper::Stop { id, force, unless_shared: false }.frame()).is_err() {
            return Err(CLOSED.into());
        }
        let answered = match answer.recv_timeout(STOP_WAIT) {
            Ok(message) => Ok(message),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(CLOSED.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut waiting = self.waiting.lock().unwrap();
                answer.try_recv().map_err(|_| {
                    waiting.remove(&id);
                    leaving.fetch_sub(1, Ordering::SeqCst);
                })
            }
        };
        match answered {
            Ok(ToApp::NotStopped { message, .. }) => Err(message),
            Ok(_) => Ok(()),
            Err(()) => Err(format!("Endeavor's helper didn't answer in {} s, so Julia may not have stopped.", STOP_WAIT.as_secs())),
        }
    }

    pub fn detach(&self) {
        self.leave();
        self.left.store(true, Ordering::SeqCst);
        let _ = self.mux.send(&ToHelper::Detach.frame());
        self.wait_end(Some(Duration::from_secs(30)));
    }

    fn wait_end(&self, timeout: Option<Duration>) {
        let (ended, changed) = &*self.ended;
        let ended = ended.lock().unwrap();
        match timeout {
            Some(timeout) => drop(changed.wait_timeout_while(ended, timeout, |ended| !*ended)),
            None => drop(changed.wait_while(ended, |ended| !*ended)),
        }
    }

    pub fn is_closed(&self) -> bool {
        *self.ended.0.lock().unwrap()
    }

    pub fn closed(&self) -> Option<Notice> {
        self.wait_end(None);
        (!self.left.load(Ordering::SeqCst)).then(|| Notice::Lost("The connection closed unexpectedly.".into()))
    }

    pub fn keep_running(&self, keep: bool) {
        let _ = self.mux.send(&ToHelper::OwnWithClient { on: !keep }.frame());
    }

    pub fn quit(&self, keep_running: bool) {
        self.leave();
        self.left.store(true, Ordering::SeqCst);
        if keep_running {
            let _ = self.mux.send(&ToHelper::Detach.frame());
            return;
        }
        let _ = self.mux.send(&ToHelper::Stop { id: self.request_id(), force: false, unless_shared: true }.frame());
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        if !self.left.swap(true, Ordering::SeqCst) {
            self.leave();
            self.input.lock().unwrap().take();
        }
    }
}

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

#[cfg(windows)]
fn exit_unreaped(child: &mut Child) {
    let _ = child.wait();
}

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
