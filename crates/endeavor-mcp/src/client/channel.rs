//! The client's end of `endeavor connect`'s stdin and stdout, on this computer
//! or over ssh: the helper's hello, file requests, and a runtime starting,
//! dying and stopping on it. It lasts as long as the helper does.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use wire::files;
use wire::relay::Mux;
use wire::slurm::JobRequest;
use wire::{Frame, ToApp, ToHelper};

use super::listener::Listener;

/// How long `stop` waits for the helper's `Stopped`.
const STOP_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 30 });

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

/// The client's end of a helper's stdin/stdout, on this computer or over ssh. It
/// lasts as long as the helper: runtimes start, die and stop on it.
pub struct Channel {
    mux: Arc<Mux>,
    /// Where control messages other than file replies go: to whoever waits on
    /// the helper now (its hello, a start, a stop, or the runtime's watcher).
    /// Replacing it ends the previous listener's wait.
    sink: Arc<Mutex<Option<mpsc::Sender<ToApp>>>>,
    hello: Mutex<Option<mpsc::Receiver<ToApp>>>,
    files: Arc<Mutex<HashMap<u32, mpsc::Sender<files::Reply>>>>,
    next_file: AtomicU32,
    /// Set once the helper has exited (or its end of the channel closed).
    ended: Arc<(Mutex<bool>, Condvar)>,
    /// The client let the helper go (a detach or a quit), so its end is no drop.
    left: AtomicBool,
    /// The listener relaying to this channel, which forgets it when it ends.
    listener: Arc<Mutex<Option<Arc<Listener>>>>,
    /// Set when the client stops the current runtime or leaves the helper, so its
    /// watcher keeps quiet.
    leaving: Mutex<Arc<AtomicBool>>,
    /// `Stopped` replies still owed to `stop`s that gave up waiting. The helper
    /// answers each `Stop` with one, which must not read as the outcome of the
    /// next `start_runtime`; the reader drops them, under this lock.
    abandoned: Arc<Mutex<u32>>,
}

impl Channel {
    /// `output` is the helper's stdout, read up to its first frame.
    pub fn open(mut helper: Child, input: impl Write + Send + 'static, output: impl Read + Send + 'static) -> Channel {
        let mux = Mux::new(input);
        let (hello_tx, hello) = mpsc::channel::<ToApp>();
        let sink = Arc::new(Mutex::new(Some(hello_tx)));
        let ended: Arc<(Mutex<bool>, Condvar)> = Arc::default();
        let listener: Arc<Mutex<Option<Arc<Listener>>>> = Arc::default();
        let files: Arc<Mutex<HashMap<u32, mpsc::Sender<files::Reply>>>> = Arc::default();
        let abandoned: Arc<Mutex<u32>> = Arc::default();
        std::thread::spawn({
            let (mux, sink, listener, files, ended, abandoned) = (mux.clone(), sink.clone(), listener.clone(), files.clone(), ended.clone(), abandoned.clone());
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
                            let mut abandoned = abandoned.lock().unwrap();
                            if message == ToApp::Stopped && *abandoned > 0 {
                                *abandoned -= 1;
                            } else if let Some(sink) = &*sink.lock().unwrap() {
                                let _ = sink.send(message);
                            }
                        }
                        Err(e) => eprintln!("The runtime helper sent an unreadable message: {e}"),
                    },
                );
                if let Some(listener) = listener.lock().unwrap().take() {
                    listener.forget(&mux);
                }
                // Whoever waits hears the end.
                sink.lock().unwrap().take();
                files.lock().unwrap().clear();
                let status = helper.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                eprintln!("The runtime helper exited ({status}){}", result.err().map(|e| format!(": {e}")).unwrap_or_default());
                *ended.0.lock().unwrap() = true;
                ended.1.notify_all();
            }
        });
        Channel {
            mux,
            sink,
            hello: Mutex::new(Some(hello)),
            files,
            next_file: AtomicU32::new(0),
            ended,
            left: AtomicBool::new(false),
            listener,
            leaving: Mutex::default(),
            abandoned,
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
    pub fn start_runtime(
        &self,
        listener: &Arc<Listener>,
        job: Option<JobRequest>,
        on_message: &mut dyn FnMut(ToApp),
        notice: impl FnOnce(Notice) + Send + 'static,
    ) -> Result<Runtime, String> {
        let events = self.subscribe();
        let leaving = Arc::new(AtomicBool::new(false));
        *self.leaving.lock().unwrap() = leaving.clone();
        self.mux.send(&ToHelper::StartRuntime { job }.frame()).map_err(|_| "The connection to Endeavor's helper closed.".to_owned())?;
        let runtime = loop {
            match events.recv() {
                Ok(message @ (ToApp::Progress { .. } | ToApp::FoundJulia { .. } | ToApp::Submitted { .. } | ToApp::Queued { .. })) => on_message(message),
                Ok(ToApp::Ready { node, pid, token, reattached, job, .. }) => {
                    *self.listener.lock().unwrap() = Some(listener.clone());
                    listener.attach(self.mux.clone(), token.clone());
                    break Runtime { port: listener.port(), mcp_url: listener.mcp_url(), page_url: listener.page_url(&token), token, pid, reattached, node, job };
                }
                Ok(ToApp::StartFailed { message } | ToApp::Error { message }) => return Err(message),
                Ok(ToApp::Died { status, log_tail }) => {
                    let how = died_reason(&status, &[]);
                    return Err(format!("Julia stopped before Pluto was ready. {how}{}{}", if how.is_empty() { "" } else { " " }, diagnose(&log_tail)));
                }
                Ok(ToApp::Stopped) => return Err("Julia was stopped while it started.".into()),
                Ok(ToApp::Replaced) => return Err("Another connection took Julia over while it was starting.".into()),
                Ok(ToApp::Hello { .. } | ToApp::Files { .. }) => {}
                Err(_) => return Err("The connection to Endeavor's helper closed.".into()),
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

    /// Stop the runtime and wait until it's gone (blocks up to ~30 s in all).
    /// The helper stays connected.
    pub fn stop(&self) {
        self.leave();
        let events = self.subscribe();
        if self.mux.send(&ToHelper::Stop.frame()).is_ok() {
            let deadline = Instant::now() + STOP_WAIT;
            while let Ok(message) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                if message == ToApp::Stopped {
                    return;
                }
            }
            let mut abandoned = self.abandoned.lock().unwrap();
            // The reader delivers under this lock, so a `Stopped` that came as the wait ended is in `events`.
            if !events.try_iter().any(|message| message == ToApp::Stopped) {
                *abandoned += 1;
            }
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
        let message = if keep_running { ToHelper::Detach } else { ToHelper::Stop };
        let _ = self.mux.send(&message.frame());
    }
}

impl Drop for Channel {
    /// A channel nobody holds any more lets the helper go, so ssh and the
    /// helper don't outlive a failed connect.
    fn drop(&mut self) {
        if !self.left.swap(true, Ordering::SeqCst) {
            self.leave();
            let _ = self.mux.send(&ToHelper::Detach.frame());
        }
    }
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
