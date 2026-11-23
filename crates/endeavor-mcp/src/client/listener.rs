use std::collections::HashSet;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use wire::relay::Mux;

use crate::guard::{AskRestart, Older, Picture, Pictures, Snapshot};

const HOLD: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 2 });

pub type Refuse = Box<dyn Fn(&str, &str, &Value) -> Option<String> + Send + Sync>;

#[derive(Clone, Copy)]
pub struct Messages {
    pub restart_failed: fn(&str) -> String,
    pub restart_needs_install: fn(&str, &[wire::Item]) -> String,
    pub not_connected: fn(&str) -> String,
    pub no_run_gate: Option<fn(&str) -> String>,
    pub tell_other_build: bool,
}

impl Default for Messages {
    fn default() -> Messages {
        Messages {
            restart_failed: |name| format!("Julia on {name} couldn't start. Use Restart Julia to try again."),
            restart_needs_install: |name, items| format!("Julia on {name} couldn't start. {} Endeavor is asking the user whether it may install that; Julia starts once they agree.", wire::needs_text(items, name)),
            not_connected: |name| format!("Endeavor isn't connected to {name}. Reconnect it to use its notebook again."),
            no_run_gate: None,
            tell_other_build: true,
        }
    }
}

pub struct Listener {
    port: u16,
    name: String,
    upstream: Mutex<Upstream>,
    changed: Condvar,
    closed: AtomicBool,
    refuse: Option<Refuse>,
    snapshot: OnceLock<Box<Pictures>>,
    restart: OnceLock<Box<AskRestart>>,
    messages: Messages,
    socket: Mutex<Option<std::sync::mpsc::Receiver<TcpListener>>>,
}

const STOP: Duration = Duration::from_secs(2);

enum Upstream {
    None,
    Up { mux: Arc<Mux>, token: String, asks: bool, other: Option<Option<u32>>, told: Arc<Mutex<HashSet<String>>> },
    Away { token: String, why: String },
}

impl Listener {
    pub fn start(name: &str) -> Result<Arc<Listener>, String> {
        Listener::new(name, None, Messages::default())
    }

    pub fn with_refuse(name: &str, refuse: Refuse) -> Result<Arc<Listener>, String> {
        Listener::new(name, Some(refuse), Messages::default())
    }

    pub fn new(name: &str, refuse: Option<Refuse>, messages: Messages) -> Result<Arc<Listener>, String> {
        Listener::on_socket(name, refuse, messages, None)
    }

    pub fn on_socket(name: &str, refuse: Option<Refuse>, messages: Messages, socket: Option<TcpListener>) -> Result<Arc<Listener>, String> {
        let socket = match socket {
            Some(socket) => socket,
            None => TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?,
        };
        let (hand_back, handed) = std::sync::mpsc::channel();
        let listener = Arc::new(Listener {
            port: socket.local_addr().map_err(|e| e.to_string())?.port(),
            name: name.to_owned(),
            upstream: Mutex::new(Upstream::None),
            changed: Condvar::new(),
            closed: AtomicBool::new(false),
            refuse,
            snapshot: OnceLock::new(),
            restart: OnceLock::new(),
            messages,
            socket: Mutex::new(Some(handed)),
        });
        let accepting = listener.clone();
        std::thread::spawn(move || {
            let mut backoff = Backoff::default();
            loop {
                let connection = socket.accept().map(|(connection, _)| connection);
                if accepting.closed.load(Ordering::SeqCst) {
                    let _ = hand_back.send(socket);
                    return;
                }
                match connection {
                    Ok(connection) => {
                        backoff.accepted();
                        let listener = accepting.clone();
                        std::thread::spawn(move || listener.route(connection));
                    }
                    Err(e) => {
                        let (pause, first) = backoff.failed();
                        if first {
                            eprintln!("Endeavor's port {} for {} couldn't take a connection: {e}", accepting.port, accepting.name);
                        }
                        std::thread::sleep(pause);
                    }
                }
            }
        });
        Ok(listener)
    }

    fn route(&self, connection: TcpStream) {
        let _ = connection.set_nodelay(true);
        let restart = self.restart.get().map(|r| (&**r, self.name.as_str()));
        let upstream = self.upstream.lock().unwrap();
        let (upstream, _) = self.changed.wait_timeout_while(upstream, HOLD, |u| matches!(u, Upstream::Away { .. })).unwrap();
        match &*upstream {
            Upstream::Up { mux, asks, other, told, .. } => {
                let (mux, asks, other, told) = (mux.clone(), *asks, *other, told.clone());
                drop(upstream);
                let no_run_gate = if asks { None } else { self.messages.no_run_gate };
                if self.refuse.is_none() && no_run_gate.is_none() && self.snapshot.get().is_none() && restart.is_none() && other.is_none() {
                    drop(mux.open(connection));
                    return;
                }
                let Ok((ours, theirs)) = loopback_pair() else { return };
                if mux.open(theirs).is_ok() {
                    let refuse = |session: &str, tool: &str, arguments: &Value| {
                        if let Some(why) = no_run_gate
                            && crate::runs_code(tool, arguments)
                        {
                            return Some(why(&self.name));
                        }
                        self.refuse.as_ref().and_then(|refuse| refuse(session, tool, arguments))
                    };
                    let how = match restart {
                        Some(_) => format!("Don't stop it yourself: call `request_restart`, which asks the user whether to restart Endeavor on {}. That closes its open notebooks, and starts this version.", self.name),
                        None => format!("Ask the user to restart Julia on {}: that closes its open notebooks, and starts this version.", self.name),
                    };
                    let no_r = || crate::no_r_text(&self.name, &how);
                    let older = other.map(|interface| Older { no_r: &no_r, notice: self.messages.tell_other_build.then(|| older_notice(&self.name, interface, &how)), told: &told });
                    let _ = crate::serve_guarded(connection, ours, &refuse, self.snapshot.get().map(|s| &**s), older.as_ref(), restart);
                }
            }
            Upstream::Away { token, why } => {
                let (token, why) = (token.clone(), why.clone());
                drop(upstream);
                let _ = connection.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = crate::serve_unreachable(connection, &token, &why, restart);
            }
            Upstream::None => {}
        }
    }

    pub fn set_snapshot(&self, snapshot: Box<Snapshot>) {
        self.set_pictures(Box::new(move |session, notebook, cell| snapshot(session, notebook, cell).map(Picture::from)));
    }

    pub fn set_pictures(&self, pictures: Box<Pictures>) {
        let _ = self.snapshot.set(pictures);
    }

    pub fn set_restart(&self, ask: Box<AskRestart>) {
        let _ = self.restart.set(ask);
    }

    pub(super) fn attach(&self, mux: Arc<Mux>, token: String, asks: bool, other: Option<Option<u32>>) {
        *self.upstream.lock().unwrap() = Upstream::Up { mux, token, asks, other, told: Arc::default() };
        self.changed.notify_all();
    }

    fn dropped(&self, why: String, mux: &Arc<Mux>) {
        let mut upstream = self.upstream.lock().unwrap();
        if let Upstream::Up { mux: up, token, .. } = &*upstream
            && Arc::ptr_eq(mux, up)
        {
            *upstream = Upstream::Away { token: token.clone(), why };
        }
    }

    fn deliberately(&self, why: String, from_up: bool) {
        let mut upstream = self.upstream.lock().unwrap();
        let token = match &*upstream {
            Upstream::Away { token, .. } => token.clone(),
            Upstream::Up { token, .. } if from_up => token.clone(),
            _ => return,
        };
        *upstream = Upstream::Away { token, why };
    }

    pub fn restarting(&self) {
        self.deliberately(format!("Endeavor is restarting Julia on {}. Try again in a moment.", self.name), true);
    }

    pub fn restart_failed(&self) {
        self.deliberately((self.messages.restart_failed)(&self.name), false);
    }

    pub fn restart_needs_install(&self, items: &[wire::Item]) {
        self.deliberately((self.messages.restart_needs_install)(&self.name, items), false);
    }

    fn not_connected(&self) -> String {
        (self.messages.not_connected)(&self.name)
    }

    pub fn disconnected(&self) {
        self.deliberately(self.not_connected(), true);
    }

    pub(super) fn forget(&self, mux: &Arc<Mux>) {
        self.dropped(format!("Endeavor lost the connection to {} and is reconnecting by itself. Try again in a moment.", self.name), mux);
    }

    pub(super) fn left(&self, mux: &Arc<Mux>) {
        self.dropped(self.not_connected(), mux);
    }

    #[cfg(test)]
    pub(super) fn asks(&self) -> Option<bool> {
        if let Upstream::Up { asks, .. } = &*self.upstream.lock().unwrap() { Some(*asks) } else { None }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn close(&self) {
        drop(self.hand_over());
    }

    pub fn hand_over(&self) -> Option<TcpListener> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return None;
        }
        *self.upstream.lock().unwrap() = Upstream::None;
        self.changed.notify_all();
        let handed = self.socket.lock().unwrap().take()?;
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        handed.recv_timeout(STOP).ok()
    }

    pub(super) fn mcp_url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub(super) fn page_url(&self, token: &str) -> String {
        format!("http://127.0.0.1:{}/?token={token}", self.port)
    }
}

struct Backoff {
    pause: Duration,
    failing: bool,
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff { pause: Backoff::FIRST, failing: false }
    }
}

impl Backoff {
    const FIRST: Duration = Duration::from_millis(50);
    const LAST: Duration = Duration::from_secs(1);

    fn failed(&mut self) -> (Duration, bool) {
        let first = !self.failing;
        let pause = self.pause;
        self.failing = true;
        self.pause = (self.pause * 2).min(Backoff::LAST);
        (pause, first)
    }

    fn accepted(&mut self) {
        *self = Backoff::default();
    }
}

fn loopback_pair() -> std::io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let ours = TcpStream::connect(listener.local_addr()?)?;
    let theirs = loop {
        let (theirs, from) = listener.accept()?;
        if from == ours.local_addr()? {
            break theirs;
        }
    };
    let _ = ours.set_nodelay(true);
    let _ = theirs.set_nodelay(true);
    Ok((ours, theirs))
}

fn older_notice(name: &str, interface: Option<u32>, how: &str) -> String {
    format!(
        "Endeavor on {name} was started by {} version, and it keeps running as it is. Some notebook tools may not work as described, or may be refused. If that happens: {how}",
        crate::which_version(interface, Some(crate::core::INTERFACE))
    )
}

#[cfg(test)]
mod tests;
