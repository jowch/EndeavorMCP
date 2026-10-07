//! The loopback port on this computer for one server's runtime: Pluto's page,
//! the agent's MCP and the app's calls all go through it, and each connection
//! is relayed to the runtime of the moment. The port stays the same while the
//! runtime goes away and comes back.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::Value;
use wire::relay::Mux;

/// How long a connection that arrives while the runtime is away waits for it
/// to come back, so a short drop goes unnoticed.
const HOLD: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 2 });

/// Why an agent's call is refused: its session key, the tool and its arguments.
pub type Refuse = Box<dyn Fn(&str, &str, &Value) -> Option<String> + Send + Sync>;

/// What a listener says when the runtime is away and nothing will bring it
/// back by itself, as a function of the server's name. The default speaks in
/// the app's words (its buttons); a caller with other controls says its own.
#[derive(Clone, Copy)]
pub struct Messages {
    /// A restart that was announced (`restarting`) did not bring Julia back.
    pub restart_failed: fn(&str) -> String,
    /// The server was stopped or disconnected on purpose.
    pub not_connected: fn(&str) -> String,
}

impl Default for Messages {
    fn default() -> Messages {
        Messages {
            restart_failed: |name| format!("Julia on {name} couldn't start. Use Restart Julia to try again."),
            not_connected: |name| format!("Endeavor isn't connected to {name}. Reconnect it to use its notebook again."),
        }
    }
}

pub struct Listener {
    port: u16,
    /// The server, as the agent is told it.
    name: String,
    upstream: Mutex<Upstream>,
    changed: Condvar,
    closed: AtomicBool,
    /// Without one, connections are relayed without reading the HTTP in them.
    refuse: Option<Refuse>,
    messages: Messages,
}

/// Where a listener's connections go.
enum Upstream {
    /// No runtime yet: a connection is closed.
    None,
    Up { mux: Arc<Mux>, token: String },
    /// The runtime was up and is being got back. A connection waits for it up
    /// to `HOLD`; then an MCP request is answered with `why`
    /// (`serve_unreachable`), and anything else closed.
    Away { token: String, why: String },
}

impl Listener {
    pub fn start(name: &str) -> Result<Arc<Listener>, String> {
        Listener::new(name, None, Messages::default())
    }

    /// A listener that parses the agent's MCP calls and fails the ones `refuse`
    /// gives a reason for (`serve_guarded`).
    pub fn with_refuse(name: &str, refuse: Refuse) -> Result<Arc<Listener>, String> {
        Listener::new(name, Some(refuse), Messages::default())
    }

    /// A listener with `refuse` (if any) and its own wording for the messages it gives.
    pub fn new(name: &str, refuse: Option<Refuse>, messages: Messages) -> Result<Arc<Listener>, String> {
        let socket = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let listener = Arc::new(Listener {
            port: socket.local_addr().map_err(|e| e.to_string())?.port(),
            name: name.to_owned(),
            upstream: Mutex::new(Upstream::None),
            changed: Condvar::new(),
            closed: AtomicBool::new(false),
            refuse,
            messages,
        });
        let accepting = listener.clone();
        std::thread::spawn(move || {
            // A failed accept (no file descriptors left, a connection reset before it was taken) doesn't end the listener.
            let mut backoff = Backoff::default();
            for connection in socket.incoming() {
                if accepting.closed.load(Ordering::SeqCst) {
                    break;
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
        let upstream = self.upstream.lock().unwrap();
        let (upstream, _) = self.changed.wait_timeout_while(upstream, HOLD, |u| matches!(u, Upstream::Away { .. })).unwrap();
        match &*upstream {
            Upstream::Up { mux, .. } => {
                let mux = mux.clone();
                drop(upstream);
                match &self.refuse {
                    Some(refuse) => {
                        let Ok((ours, theirs)) = loopback_pair() else { return };
                        if mux.open(theirs).is_ok() {
                            let _ = crate::serve_guarded(connection, ours, &|session, tool, arguments| refuse(session, tool, arguments));
                        }
                    }
                    None => drop(mux.open(connection)),
                }
            }
            Upstream::Away { token, why } => {
                let (token, why) = (token.clone(), why.clone());
                drop(upstream);
                let _ = connection.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = crate::serve_unreachable(connection, &token, &why);
            }
            Upstream::None => {}
        }
    }

    pub(super) fn attach(&self, mux: Arc<Mux>, token: String) {
        *self.upstream.lock().unwrap() = Upstream::Up { mux, token };
        self.changed.notify_all();
    }

    /// The runtime is away while `why` holds, if it was up and is `mux`'s. A
    /// runtime that is already away keeps the reason it has: a drop never
    /// replaces a deliberate one.
    fn dropped(&self, why: String, mux: &Arc<Mux>) {
        let mut upstream = self.upstream.lock().unwrap();
        if let Upstream::Up { mux: up, token } = &*upstream
            && Arc::ptr_eq(mux, up)
        {
            *upstream = Upstream::Away { token: token.clone(), why };
        }
    }

    /// The runtime is away on purpose while `why` holds, whether it was away
    /// for another reason or, if `from_up`, up.
    fn deliberately(&self, why: String, from_up: bool) {
        let mut upstream = self.upstream.lock().unwrap();
        let token = match &*upstream {
            Upstream::Away { token, .. } => token.clone(),
            Upstream::Up { token, .. } if from_up => token.clone(),
            _ => return,
        };
        *upstream = Upstream::Away { token, why };
    }

    /// The runtime is being restarted.
    pub fn restarting(&self) {
        self.deliberately(format!("Endeavor is restarting Julia on {}. Try again in a moment.", self.name), true);
    }

    /// The restart `restarting` announced didn't work out: Julia didn't come back.
    pub fn restart_failed(&self) {
        self.deliberately((self.messages.restart_failed)(&self.name), false);
    }

    fn not_connected(&self) -> String {
        (self.messages.not_connected)(&self.name)
    }

    /// The server was stopped or disconnected on purpose: nothing will
    /// reconnect it by itself, unlike a drop (`forget`).
    pub fn disconnected(&self) {
        self.deliberately(self.not_connected(), true);
    }

    /// `mux`'s helper has gone, unexpectedly.
    pub(super) fn forget(&self, mux: &Arc<Mux>) {
        self.dropped(format!("Endeavor lost the connection to {} and is reconnecting by itself. Try again in a moment.", self.name), mux);
    }

    /// `mux`'s helper has gone because the client let it go, so nothing will reconnect it.
    pub(super) fn left(&self, mux: &Arc<Mux>) {
        self.dropped(self.not_connected(), mux);
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop taking connections and close the port. A connection already relayed goes on until its ends close.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.upstream.lock().unwrap() = Upstream::None;
        self.changed.notify_all();
        // The accept loop is woken by a connection of its own.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }

    /// The MCP URL the agent's config carries; the same for the listener's whole life.
    pub(super) fn mcp_url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Pluto's start page: the core trades `token` in the URL for a cookie, and drops it from the URL.
    pub(super) fn page_url(&self, token: &str) -> String {
        format!("http://127.0.0.1:{}/?token={token}", self.port)
    }
}

/// How long the accept loop waits after a failed accept: 50 ms, doubling at each
/// failure in a row up to a second, and 50 ms again once one succeeds.
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

    /// How long to wait now, and whether this is the first failure of a run, which is the one to report.
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

/// Two ends of a loopback connection: one to read and write here, one for the relay.
fn loopback_pair() -> std::io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let ours = TcpStream::connect(listener.local_addr()?)?;
    // Another process may reach the temporary port first.
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

#[cfg(test)]
mod tests;
