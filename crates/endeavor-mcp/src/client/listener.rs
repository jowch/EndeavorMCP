//! The loopback port on this computer for one server's runtime: Pluto's page,
//! the agent's MCP and the app's calls all go through it, and each connection
//! is relayed to the runtime of the moment. The port stays the same while the
//! runtime goes away and comes back.

use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::Value;
use wire::relay::Mux;

/// How long a connection that arrives while the runtime is away waits for it
/// to come back, so a short drop goes unnoticed.
const HOLD: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 2 });

/// Why an agent's call is refused: its session key, the tool and its arguments.
pub type Refuse = Box<dyn Fn(&str, &str, &Value) -> Option<String> + Send + Sync>;

pub struct Listener {
    port: u16,
    /// The server, as the agent is told it.
    name: String,
    upstream: Mutex<Upstream>,
    changed: Condvar,
    /// Without one, connections are relayed without reading the HTTP in them.
    refuse: Option<Refuse>,
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
        Listener::open(name, None)
    }

    /// A listener that parses the agent's MCP calls and fails the ones `refuse`
    /// gives a reason for (`serve_guarded`).
    pub fn with_refuse(name: &str, refuse: Refuse) -> Result<Arc<Listener>, String> {
        Listener::open(name, Some(refuse))
    }

    fn open(name: &str, refuse: Option<Refuse>) -> Result<Arc<Listener>, String> {
        let socket = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let listener = Arc::new(Listener {
            port: socket.local_addr().map_err(|e| e.to_string())?.port(),
            name: name.to_owned(),
            upstream: Mutex::new(Upstream::None),
            changed: Condvar::new(),
            refuse,
        });
        let accepting = listener.clone();
        std::thread::spawn(move || {
            // A failed accept (no file descriptors left, a connection reset before it was taken) doesn't end the listener.
            for connection in socket.incoming() {
                match connection {
                    Ok(connection) => {
                        let listener = accepting.clone();
                        std::thread::spawn(move || listener.route(connection));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
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

    /// The runtime is away on purpose while `why` holds, whether it was up or
    /// away for another reason.
    fn deliberately(&self, why: String) {
        let mut upstream = self.upstream.lock().unwrap();
        if let Upstream::Up { token, .. } | Upstream::Away { token, .. } = &*upstream {
            *upstream = Upstream::Away { token: token.clone(), why };
        }
    }

    /// The runtime is being restarted.
    pub fn restarting(&self) {
        self.deliberately(format!("Endeavor is restarting Julia on {}. Try again in a moment.", self.name));
    }

    /// The restart `restarting` announced didn't work out: Julia didn't come back.
    pub fn restart_failed(&self) {
        let mut upstream = self.upstream.lock().unwrap();
        if let Upstream::Away { token, .. } = &*upstream {
            *upstream = Upstream::Away { token: token.clone(), why: format!("Julia on {} couldn't start. Use Restart Julia to try again.", self.name) };
        }
    }

    /// The server was stopped or disconnected on purpose: nothing will
    /// reconnect it by itself, unlike a drop (`forget`).
    pub fn disconnected(&self) {
        self.deliberately(format!("Endeavor isn't connected to {}. Reconnect it to use its notebook again.", self.name));
    }

    /// `mux`'s helper has gone.
    pub(super) fn forget(&self, mux: &Arc<Mux>) {
        self.dropped(format!("Endeavor lost the connection to {} and is reconnecting by itself. Try again in a moment.", self.name), mux);
    }

    pub fn port(&self) -> u16 {
        self.port
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
