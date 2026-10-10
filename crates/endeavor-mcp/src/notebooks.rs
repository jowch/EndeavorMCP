//! What the core keeps about each open notebook (docs/runtime-core.md): the
//! notebook tools' rules (see `tools`), staging and read receipts, who last
//! changed each cell and the code the agent's edits replaced, idle stop, each
//! agent session's one notebook, and the `/events` stream the app follows.
//!
//! The engine's adapter (Julia's, for Pluto) answers calls on its
//! `POST /adapter` (`snapshot`, `graph`, `apply`, `run` and the rest) and says
//! when a notebook changed on its `GET /notifications` stream; the core reads a
//! fresh snapshot each time it tells the app anything. A notebook's state goes
//! when it shuts down, except its entry in `idle_stopped`.

mod engines;
mod r;
mod tools;

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::asks::Asks;
use crate::host_tools::home;
#[cfg(unix)]
use crate::host_tools::normpath;
use crate::http::{self, Head};
use crate::mcp::{WRITE_TOOLS, julia_string, to_json};
use wire::backend::Backend;

pub use engines::Engines;
pub use r::R;
pub use tools::{Reply, WAIT_SECONDS};

const IDLE_CHECK: Duration = Duration::from_secs(300);

/// Hours without activity after which a notebook stops, unless the caller sets another.
pub const IDLE_HOURS: f64 = 48.0;

/// How often to look for idle notebooks. ENDEAVOR_IDLE_CHECK_SECS: tests look
/// more often than every five minutes.
pub fn idle_check() -> Duration {
    std::env::var("ENDEAVOR_IDLE_CHECK_SECS").ok().and_then(|s| s.parse().ok()).map_or(IDLE_CHECK, Duration::from_secs_f64)
}

/// The engine's adapter, once it answers.
pub trait Upstream: Send + Sync {
    /// The reply to one `POST /adapter` call.
    fn adapter(&self, raw: &[u8]) -> io::Result<String>;
    /// The engine's notification stream, from its start.
    fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>>;
}

/// Starts an engine's adapter: what answers for it once it's up, or why it couldn't start.
pub type Starter = Box<dyn Fn(Backend) -> Result<Arc<dyn Upstream>, String> + Send + Sync>;

/// Julia's bridge, once it answers.
pub struct Julia {
    pub port: OnceLock<u16>,
    token: String,
}

impl Julia {
    pub fn new(token: String) -> Julia {
        Julia { port: OnceLock::new(), token }
    }
}

impl Upstream for Julia {
    fn adapter(&self, raw: &[u8]) -> io::Result<String> {
        let port = *self.port.get().ok_or(io::ErrorKind::NotConnected)?;
        let authorization = format!("Bearer {}", self.token);
        let headers = [("Authorization", authorization.as_str()), ("Content-Type", "application/json")];
        let (status, body) = http::post(port, "/adapter", &headers, raw)?;
        if status != 200 {
            return Err(io::Error::other(format!("Julia's /adapter answered {status}")));
        }
        String::from_utf8(body).map_err(|_| io::ErrorKind::InvalidData.into())
    }

    fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>> {
        let port = *self.port.get().ok_or(io::ErrorKind::NotConnected)?;
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        // HTTP/1.0: the stream runs to the end of the connection, unchunked.
        write!(stream, "GET /notifications HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\n\r\n", self.token)?;
        let mut reader = BufReader::new(stream);
        let head = Head::read(&mut reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if head.status() != 200 {
            return Err(io::Error::other(format!("Julia's /notifications answered {}", head.status())));
        }
        Ok(Box::new(reader))
    }
}

/// A change to a cell through the tools.
struct Change {
    owner: String,
    seq: u64,
}

#[derive(Default)]
struct NotebookState {
    path: String,
    /// Each cell's code (hashed) as last seen, and who made it so: "agent",
    /// "user", or "" if it hasn't changed since the core first saw it.
    authors: HashMap<String, (u64, &'static str)>,
    /// Each unrun cell's code before the agent's first edit since it last ran.
    befores: HashMap<String, String>,
    /// The engine's `seq` after the agent's last edit of each cell: a snapshot
    /// or notification numbered below it may still show the code from before.
    edited_at: HashMap<String, u64>,
    last_active: Option<f64>,
    kept_alive: bool,
    /// Cells edited through the tools and not run since, with when. A cell
    /// stops being pending once it runs, however it runs (the tools, Pluto's
    /// own run button, a reactive re-run).
    pending: HashMap<String, f64>,
    /// When the tools last changed each cell, kept until the tools run it. A
    /// cell here that has run since ran the agent's code some other way (the
    /// user's run reached it), so an approved run of it needn't run it again.
    tool_edits: HashMap<String, f64>,
    /// Cells the user's run reached after the page asked first (Run anyway),
    /// with their `last_run` before it and when the app said so: an approved
    /// run of them that comes right after needn't run them again, edited by
    /// the tools or not.
    user_runs: HashMap<String, (f64, f64)>,
    /// What each agent session last read of each cell: (owner, cell) => (code,
    /// seq). Per owner, so one session's reads and edits aren't another's.
    reads: HashMap<(String, String), (String, u64)>,
    /// Which session last changed each cell through the tools, and when.
    changes: HashMap<String, Change>,
}

impl NotebookState {
    /// Whether a session read or changed one of its cells after the state's `seq` was `since`.
    fn touched_after(&self, since: u64) -> bool {
        self.reads.values().map(|(_, seq)| *seq).chain(self.changes.values().map(|c| c.seq)).any(|seq| seq > since)
    }

    /// Code that changed without the agent's tools writing it was changed by
    /// the user. Code the engine read (at `seq`) before the agent's last edit
    /// of the cell says nothing new.
    fn author(&mut self, cell: &str, code: &str, seq: Option<u64>) -> Option<&'static str> {
        if let (Some(seq), Some(&edited)) = (seq, self.edited_at.get(cell))
            && seq < edited
        {
            return self.authors.get(cell).map(|&(_, author)| author).filter(|a| !a.is_empty());
        }
        let hash = hash(code);
        let author = match self.authors.get(cell) {
            None => "",
            Some(&(known, author)) if known == hash => author,
            Some(_) => "user",
        };
        self.authors.insert(cell.to_owned(), (hash, author));
        Some(author).filter(|a| !a.is_empty())
    }

    /// The before-text while the cell is unrun and differs; forgotten once it runs.
    fn before(&mut self, cell: &str, code: &str, unrun: bool) -> Option<String> {
        if !unrun {
            self.befores.remove(cell);
            return None;
        }
        self.befores.get(cell).filter(|before| *before != code).cloned()
    }

    /// The agent's tools just wrote this cell's code; `before` is what it
    /// replaced, `seq` the engine's after the change.
    fn agent_edited(&mut self, cell: &str, before: &str, code: &str, seq: Option<u64>) {
        self.authors.insert(cell.to_owned(), (hash(code), "agent"));
        if let Some(seq) = seq {
            self.edited_at.insert(cell.to_owned(), seq);
        }
        self.befores.entry(cell.to_owned()).or_insert_with(|| before.to_owned());
    }

    /// The pending cells, in notebook order, then any no longer in it (a cell
    /// Pluto's page or a reload removed stays pending until a run prunes it).
    /// Cells that ran since their edit stop being pending.
    fn pending_run(&mut self, nb: &Snapshot) -> Vec<String> {
        self.pending.retain(|id, edited| !nb.cells.get(id).is_some_and(|c| c.ran_since(*edited)));
        let mut ids: Vec<String> = nb.order.iter().filter(|id| self.pending.contains_key(*id)).cloned().collect();
        let mut gone: Vec<String> = self.pending.keys().filter(|id| !nb.cells.contains_key(*id)).cloned().collect();
        gone.sort();
        ids.extend(gone);
        ids
    }

    /// Forget pending cells no longer in the notebook.
    fn prune(&mut self, nb: &Snapshot) {
        self.pending.retain(|id, _| nb.cells.contains_key(id));
    }
}

struct State {
    notebooks: HashMap<String, NotebookState>,
    /// Orders reads against changes; two can share a clock reading.
    seq: u64,
    idle_limit_hours: f64,
    /// Notebooks the idle check stopped, by canonical path, until they open
    /// again: the app reads these to say why a notebook stopped.
    idle_stopped: Vec<(String, Value)>,
    /// One notebook per agent session: owner => the canonical path it works on.
    /// The app binds a session started from an existing notebook; otherwise
    /// the first notebook the session opens or creates binds it.
    bindings: HashMap<String, String>,
    /// When each agent session last made a tool call, for `recent_sessions` and
    /// to forget sessions that went quiet. Made when a session is bound or
    /// makes a call; dropped with its binding, or `SESSION_KEPT` after its
    /// last call or its binding (and then its binding goes too).
    seen: HashMap<String, Seen>,
}

struct Seen {
    /// The time of its last `tools/call`; nothing else counts.
    last_call: Option<f64>,
    /// When the record was made.
    since: f64,
}

/// How long a session's record outlasts its last call.
const SESSION_KEPT: f64 = 7.0 * 24.0 * 3600.0;

impl State {
    /// A session's record, made now if it has none. Old records go when a new one is made.
    fn record(&mut self, owner: &str, now: f64) -> &mut Seen {
        if !self.seen.contains_key(owner) {
            self.forget_old(now);
        }
        self.seen.entry(owner.to_owned()).or_insert(Seen { last_call: None, since: now })
    }

    /// A session is bound or opens a notebook: its record is kept from now on, whether it was old or not.
    fn record_bound(&mut self, owner: &str, now: f64) {
        self.record(owner, now).since = now;
    }

    /// Drop the records of sessions that haven't called in a week, and their bindings.
    fn forget_old(&mut self, now: f64) {
        let bindings = &mut self.bindings;
        self.seen.retain(|session, seen| {
            let kept = now - seen.last_call.unwrap_or(f64::MIN).max(seen.since) < SESSION_KEPT;
            if !kept {
                bindings.remove(session);
            }
            kept
        });
    }
}

#[derive(Default)]
struct Events {
    subscribers: Vec<Sender<String>>,
    last: String,
}

pub struct Notebooks {
    engines: Arc<Engines>,
    /// Where each engine's notifications go, once `start` has run.
    notify: OnceLock<Sender<Value>>,
    /// Starts an engine other than Pluto's, the first time one of its notebooks is opened or made (the core sets it).
    pub starter: OnceLock<Starter>,
    /// Held while an engine starts, so it starts once.
    starting: Mutex<()>,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    state: Mutex<State>,
    /// Held while reading the engine's state and telling the app, so events go out in order.
    publishing: Mutex<()>,
    events: Mutex<Events>,
    /// The app build this runtime came from, which the app compares with its own.
    pub build: OnceLock<String>,
    /// Whether the runtime ends itself once no notebook has been open for the idle limit (`core::exit_when_idle`).
    pub exits_when_idle: bool,
    /// Runs waiting for the user's answer.
    pub asks: Asks,
}

/// One notebook as the engine's `snapshot` reports it.
struct Snapshot {
    /// The engine's `seq` when it read the notebook.
    seq: Option<u64>,
    id: String,
    path: String,
    order: Vec<String>,
    execution_allowed: bool,
    safe_preview: bool,
    process_status: Value,
    /// Its own Julia process ended by itself (not stopped by Pluto or the
    /// app) and hasn't been restarted: the cells that were running then.
    exited: Option<Vec<String>>,
    /// Pluto's package step while one is under way (`package_step` in the
    /// runtime): the cells wait, queued, until it ends.
    packages: Option<Value>,
    cells: HashMap<String, Cell>,
}

struct Cell {
    code: String,
    folded: bool,
    running: bool,
    queued: bool,
    errored: bool,
    /// When its last run ended (Unix seconds, 0 if it never ran) and how long
    /// it took (nanoseconds).
    last_run: f64,
    runtime: f64,
    /// Its output as the tools show it, and the structured error if it errored.
    output: String,
    error: Option<Value>,
    /// Boilerplate the tools don't show (Pluto's package cells and the like).
    hidden: bool,
    markdown: bool,
    /// What the engine itself knows: a result made before an ancestor last ran
    /// (Ember's), and a cell that hasn't run since the notebook started (Ember's
    /// restart leaves every cell so). Pluto's are always false.
    stale: bool,
    not_run: bool,
}

impl Cell {
    /// A run that started after `edited` and has finished. The engine stamps a
    /// run when it ends, so a run already under way at the edit (of the old
    /// code) ends after it; its duration gives its start.
    fn ran_since(&self, edited: f64) -> bool {
        !(self.running || self.queued) && self.last_run > 0.0 && self.last_run - self.runtime / 1e9 >= edited
    }
}

impl Snapshot {
    fn parse(value: &Value) -> Option<Snapshot> {
        let text = |v: &Value| v.as_str().map(str::to_owned);
        let flag = |v: &Value| v.as_bool().unwrap_or(false);
        let cells = value["cells"].as_array()?.iter().map(|c| {
            let cell = Cell {
                code: text(&c["code"])?,
                folded: flag(&c["folded"]),
                running: flag(&c["running"]),
                queued: flag(&c["queued"]),
                errored: flag(&c["errored"]),
                last_run: c["last_run"].as_f64().unwrap_or(0.0),
                runtime: c["runtime"].as_f64().unwrap_or(0.0),
                output: text(&c["output"]).unwrap_or_default(),
                error: c.get("error").filter(|e| !e.is_null()).cloned(),
                hidden: flag(&c["hidden"]),
                markdown: flag(&c["markdown"]),
                stale: flag(&c["stale"]),
                not_run: flag(&c["not_run"]),
            };
            Some((text(&c["cell_id"])?, cell))
        });
        Some(Snapshot {
            seq: value["seq"].as_u64(),
            id: text(&value["notebook_id"])?,
            path: text(&value["path"])?,
            order: value["cell_order"].as_array()?.iter().filter_map(text).collect(),
            execution_allowed: flag(&value["execution_allowed"]),
            safe_preview: flag(&value["safe_preview"]),
            process_status: value["process_status"].clone(),
            exited: value["exited"].as_array().map(|ids| ids.iter().filter_map(text).collect()),
            packages: value.get("packages").filter(|p| p.is_object()).cloned(),
            cells: cells.collect::<Option<_>>()?,
        })
    }

    /// Pluto marks every cell `queued` when it loads a notebook, ahead of the
    /// planned run; in safe preview that run never happens, so the flag
    /// lingers. A queued cell only counts while the notebook is running
    /// something or installing the packages its run needs.
    fn is_running(&self, cell: &Cell) -> bool {
        cell.running || (cell.queued && (self.packages.is_some() || self.cells.values().any(|c| c.running)))
    }

    /// Whether its cells wait on a package step: one is under way and a cell is queued.
    fn installing(&self) -> bool {
        self.packages.is_some() && self.cells.values().any(|c| c.queued)
    }

    /// What `list_notebooks` says of it; `this_session`: it's the caller's
    /// own notebook. `exited` only while its own Julia has ended by itself.
    fn summary(&self, pending_run: &[String], this_session: bool) -> Value {
        let running: Vec<&String> = self.order.iter().filter(|id| self.cells.get(*id).is_some_and(|c| self.is_running(c))).collect();
        let mut summary = json!({
            "notebook_id": self.id, "path": self.path, "cell_count": self.order.len(),
            "pending_run": pending_run, "running": running, "execution_allowed": self.execution_allowed,
            "this_session": this_session,
        });
        if let Some(exited) = &self.exited {
            summary["exited"] = json!({ "running": exited });
        }
        if let Some(packages) = &self.packages {
            summary["packages"] = packages.clone();
        }
        summary
    }
}

/// A notebook's dependency graph as the engine last analysed it.
#[derive(Default)]
struct Graph {
    /// In the engine's order.
    cells: Vec<Node>,
    /// The cells that can run, in the order they run.
    order: Vec<String>,
    /// Cells that can't run: in a cycle, or defining what another cell does.
    errable: Vec<String>,
}

struct Node {
    id: String,
    definitions: Vec<String>,
    functions: Vec<String>,
    references: Vec<String>,
    /// The cells this one depends on directly, and those depending on it.
    upstream: Vec<String>,
    downstream: Vec<String>,
    /// The packages it loads, sorted.
    packages: Vec<String>,
}

/// What to ask of `graph`: a fresh analysis of the notebook as it is now,
/// Pluto's page's dependency cache brought up to date first, each cell's
/// edges, each cell's packages.
#[derive(Clone, Copy, Default)]
struct GraphQuery {
    fresh: bool,
    refresh: bool,
    edges: bool,
    packages: bool,
}

impl Graph {
    fn parse(value: &Value) -> Graph {
        let names = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
        // Pluto's analysis names each anonymous function `__ExprExpl_anon__<random>`.
        let defined = |v: &Value| names(v).into_iter().filter(|n| !n.starts_with("__ExprExpl_anon__")).collect::<Vec<_>>();
        let cells = value["cells"].as_array().into_iter().flatten().filter_map(|c| {
            Some(Node {
                id: c["cell_id"].as_str()?.to_owned(),
                definitions: defined(&c["definitions"]),
                functions: defined(&c["functions"]),
                references: names(&c["references"]),
                upstream: names(&c["upstream"]),
                downstream: names(&c["downstream"]),
                packages: names(&c["packages"]),
            })
        });
        Graph { cells: cells.collect(), order: names(&value["order"]), errable: names(&value["errable"]) }
    }

    fn node(&self, id: &str) -> Option<&Node> {
        self.cells.iter().find(|n| n.id == id)
    }

    /// What a cell defines, as it's named to the user: up to three names.
    fn name(&self, id: &str) -> Option<String> {
        let node = self.node(id)?;
        let mut defs: Vec<&str> = node.definitions.iter().chain(&node.functions).map(String::as_str).collect();
        defs.sort_unstable();
        defs.dedup();
        (!defs.is_empty()).then(|| defs[..defs.len().min(3)].join(", "))
    }

    /// Every cell the given ones depend on, directly or not.
    fn upstream_of(&self, from: &[String]) -> HashSet<String> {
        self.closure(from, |n| &n.upstream)
    }

    /// Every cell depending on the given ones, directly or not.
    fn downstream_of(&self, from: &[String]) -> HashSet<String> {
        self.closure(from, |n| &n.downstream)
    }

    fn closure(&self, from: &[String], next: impl Fn(&Node) -> &Vec<String>) -> HashSet<String> {
        let mut found = HashSet::new();
        let mut todo: Vec<&String> = from.iter().collect();
        while let Some(id) = todo.pop() {
            for other in self.node(id).map(&next).into_iter().flatten() {
                if found.insert(other.clone()) {
                    todo.push(other);
                }
            }
        }
        found
    }

    /// The order a whole run goes in: runnable cells, then the rest.
    fn execution_order(&self) -> Vec<String> {
        let mut order = self.order.clone();
        order.extend(self.errable.iter().filter(|id| !self.order.contains(id)).cloned());
        order
    }
}

impl Notebooks {
    /// With Pluto's engine (`pluto`) if Julia runs already; else it joins when Julia starts.
    pub fn new(pluto: Option<Arc<dyn Upstream>>, clock: Box<dyn Fn() -> f64 + Send + Sync>) -> Notebooks {
        Notebooks {
            asks: Asks::new(clock()),
            engines: Arc::new(Engines::new(pluto)),
            notify: OnceLock::new(),
            starter: OnceLock::new(),
            starting: Mutex::default(),
            clock,
            state: Mutex::new(State { notebooks: HashMap::new(), seq: 0, idle_limit_hours: IDLE_HOURS, idle_stopped: Vec::new(), bindings: HashMap::new(), seen: HashMap::new() }),
            publishing: Mutex::default(),
            events: Mutex::default(),
            build: OnceLock::new(),
            exits_when_idle: false,
        }
    }

    /// Follow the engine's notifications and check for idle notebooks, once it answers.
    pub fn start(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel();
        for (backend, upstream) in self.engines.parts() {
            self.follow(backend, upstream, tx.clone());
        }
        let _ = self.notify.set(tx);
        let notebooks = self.clone();
        std::thread::spawn(move || notebooks.handle_notifications(rx));
        let every = idle_check();
        let notebooks = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(every);
                notebooks.stop_idle();
            }
        });
    }

    /// An engine, once it runs: calls for its notebooks go to it, and its notifications are followed.
    /// Nothing if it's there already.
    pub fn add_engine(&self, backend: Backend, upstream: Arc<dyn Upstream>) {
        if self.engines.add(backend, upstream.clone())
            && let Some(tx) = self.notify.get()
        {
            self.follow(backend, upstream, tx.clone());
        }
    }

    /// The engine notebook `id` is open in.
    pub fn backend_of(&self, id: &str) -> Backend {
        self.engines.backend_of(id)
    }

    /// Start `backend`'s engine unless it runs. Julia keeps its own start in order (core.rs), and can
    /// take minutes; the other engines' starts are taken one at a time here.
    fn start_engine(&self, backend: Backend) -> Result<(), String> {
        if self.engines.has(backend) {
            return Ok(());
        }
        let _starting = (backend != Backend::Pluto).then(|| self.starting.lock().unwrap());
        if self.engines.has(backend) {
            return Ok(());
        }
        let Some(starter) = self.starter.get() else { return Ok(()) };
        let upstream = starter(backend)?;
        self.add_engine(backend, upstream);
        Ok(())
    }

    /// Pass one engine's notifications on to `tx`, from each (re)connection of its stream on.
    fn follow(&self, backend: Backend, upstream: Arc<dyn Upstream>, tx: Sender<Value>) {
        let engines = self.engines.clone();
        std::thread::spawn(move || {
            // Until another engine than Pluto's is dropped or replaced: a new one has its own follower.
            while engines.is_current(backend, &upstream) {
                let stream = match upstream.notifications() {
                    Ok(stream) => stream,
                    Err(_) if backend != Backend::Pluto && let Err(e) = upstream.adapter(br#"{"method":"status","params":{}}"#) => {
                        // Not answering at all: its notebooks went with it.
                        eprintln!("{} notebooks' engine stopped answering ({e})", engines::language(backend));
                        engines.drop_engine(backend, &upstream);
                        let _ = tx.send(json!({ "method": "resync" }));
                        return;
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                {
                    // What changed while no stream was open.
                    let _ = tx.send(json!({ "method": "resync" }));
                    for line in stream.lines().map_while(Result::ok) {
                        if let Some(message) = line.strip_prefix("data: ").and_then(|m| serde_json::from_str::<Value>(m).ok()) {
                            let id = message["params"]["notebook_id"].as_str().unwrap_or_default();
                            match message["method"].as_str() {
                                Some("notebook_opened") => engines.learn(id, backend),
                                Some("notebook_shut_down") => engines.forget(id),
                                _ => {}
                            }
                            let _ = tx.send(message);
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    /// Act on notifications as they come, telling the app once per burst.
    fn handle_notifications(self: Arc<Self>, rx: Receiver<Value>) {
        while let Ok(first) = rx.recv() {
            let mut publish = self.notified(&first);
            while let Ok(next) = rx.try_recv() {
                publish |= self.notified(&next);
            }
            if publish {
                self.publish();
            }
        }
    }

    /// Act on one notification from the engine; whether the app should hear now.
    fn notified(self: &Arc<Self>, message: &Value) -> bool {
        let params = &message["params"];
        let id = params["notebook_id"].as_str().unwrap_or_default().to_owned();
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap();
        match message["method"].as_str().unwrap_or_default() {
            "notebook_opened" => {
                state.notebooks.entry(id).or_default().last_active = Some(now);
                if let Ok(path) = canonical_path(params["path"].as_str().unwrap_or_default()) {
                    state.idle_stopped.retain(|(p, _)| *p != path);
                }
                true
            }
            "notebook_shut_down" => {
                state.notebooks.remove(&id);
                // Pluto is still tidying up when it says so.
                let notebooks = self.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(200));
                    notebooks.publish();
                });
                false
            }
            "file_saved" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    notebook.last_active = Some(now);
                }
                false
            }
            "execution_done" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    notebook.last_active = Some(now);
                }
                true
            }
            // The cells a run the tools didn't wait for finished: no longer pending.
            "run_finished" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    for cell in params["cells"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                        notebook.pending.remove(cell);
                    }
                }
                true
            }
            // Seen as each state comes, so an edit made and undone between two
            // events still counts as the user's.
            "cell_state" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    for cell in params["cells"].as_array().into_iter().flatten() {
                        if let (Some(cell), Some(code)) = (cell["cell_id"].as_str(), cell["code"].as_str()) {
                            notebook.author(cell, code, message["seq"].as_u64());
                        }
                    }
                }
                true
            }
            "topology_changed" | "process_exited" | "resync" => true,
            _ => false,
        }
    }

    /// One call to the engine's adapter: its result, or the error it raised.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        if matches!(method, "open" | "new") {
            // A new notebook without a path is Pluto's.
            self.start_engine(params["path"].as_str().map_or(Backend::Pluto, engines::of_path))?;
        }
        let message = json!({ "method": method, "params": params });
        let reply = self.engines.adapter(message.to_string().as_bytes()).map_err(|e| e.to_string())?;
        let mut reply: Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
        match reply.get("error") {
            Some(error) => Err(julia_string(error)),
            None => Ok(reply["result"].take()),
        }
    }

    /// Every open notebook, in the engine's order.
    fn snapshots(&self) -> Result<Vec<Snapshot>, String> {
        let all = self.call("snapshot", json!({}))?;
        let parse = |nb: &Value| Snapshot::parse(nb).map(|nb| Snapshot { seq: nb.seq.or(all["seq"].as_u64()), ..nb }).ok_or_else(|| format!("bad snapshot {nb}"));
        all["notebooks"].as_array().ok_or("snapshot has no notebooks")?.iter().map(parse).collect()
    }

    fn snapshot(&self, id: &str) -> Result<Snapshot, String> {
        Snapshot::parse(&self.call("snapshot", json!({ "notebook_id": id }))?).ok_or_else(|| "bad snapshot".into())
    }

    fn graph(&self, id: &str, query: GraphQuery) -> Result<Graph, String> {
        let params = json!({ "notebook_id": id, "fresh": query.fresh, "refresh": query.refresh, "edges": query.edges, "packages": query.packages });
        Ok(Graph::parse(&self.call("graph", params)?))
    }

    /// A notebook's state, made if the core hasn't seen it yet.
    fn with_state<T>(&self, id: &str, f: impl FnOnce(&mut NotebookState) -> T) -> T {
        f(self.state.lock().unwrap().notebooks.entry(id.to_owned()).or_default())
    }

    /// Every notebook's state as the app hears it, with what the core keeps
    /// brought up to date:
    ///   {"notebooks": [the list_notebooks summary],
    ///    "cells": {notebook_id: [{cell_id, running, errored, unrun, author, before, version, name}, ...]},
    ///    "idle_stopped": [{path, hours, safe_preview}, ...],
    ///    "asks": [{id, owner, call_id, tool, arguments, since}, ...],
    ///    "build": the app build this runtime came from, if it was told}
    /// with cells in notebook order. `unrun`: edited by the agent and not run
    /// since. `author`: who last changed the cell's code ("agent", "user", or
    /// null if unchanged since the core first saw it). `before`: an unrun
    /// cell's code before the agent's first edit since it last ran ("" for a
    /// cell the agent added), for the in-editor diff; null otherwise.
    /// `version`: a hash of the code, so the app sees each edit. `name`: what
    /// the cell defines, as of its last run (null if nothing).
    fn compose(&self) -> Result<String, String> {
        // The snapshots are taken before the state is locked, so a notebook made meanwhile is missing from them,
        // though a tool may already have recorded reads of it (new_notebook does). Its state stays.
        let since = self.state.lock().unwrap().seq;
        let snapshots = self.snapshots()?;
        let graphs = snapshots.iter().map(|nb| Ok((nb.id.clone(), self.graph(&nb.id, GraphQuery::default())?))).collect::<Result<HashMap<_, _>, String>>()?;
        let mut state = self.state.lock().unwrap();
        state.notebooks.retain(|id, notebook| snapshots.iter().any(|nb| nb.id == *id) || notebook.touched_after(since));
        let mut list = Vec::new();
        let mut cells = Map::new();
        for nb in &snapshots {
            let notebook = state.notebooks.entry(nb.id.clone()).or_default();
            notebook.path = nb.path.clone();
            let cell = |id: &String| nb.cells.get(id).ok_or_else(|| format!("no cell {id} in {}", nb.id));
            let pending = notebook.pending_run(nb);
            list.push(nb.summary(&pending, false));
            let mut states = Vec::new();
            for id in &nb.order {
                let c = cell(id)?;
                let unrun = pending.contains(id);
                states.push(json!({
                    "cell_id": id,
                    "running": nb.is_running(c),
                    "errored": c.errored,
                    "unrun": unrun,
                    "author": notebook.author(id, &c.code, nb.seq),
                    "before": notebook.before(id, &c.code, unrun),
                    "version": format!("{:x}", hash(&c.code)),
                    "name": graphs[&nb.id].name(id),
                }));
            }
            cells.insert(nb.id.clone(), Value::Array(states));
        }
        let idle_stopped: Vec<Value> = state.idle_stopped.iter().map(|(_, entry)| entry.clone()).collect();
        let mut event = json!({ "notebooks": list, "cells": cells, "idle_stopped": idle_stopped, "asks": self.asks.list() });
        if let Some(build) = self.build.get() {
            event["build"] = build.clone().into();
        }
        Ok(to_json(&event))
    }

    /// Tell the app the notebooks' state if it changed since the last time.
    pub fn publish(&self) {
        let _publishing = self.publishing.lock().unwrap();
        let json = match self.compose() {
            Ok(json) => json,
            Err(e) => return eprintln!("┌ Warning: Couldn't summarize notebooks for the app: {e}"),
        };
        let mut events = self.events.lock().unwrap();
        if json != events.last {
            events.last = json.clone();
            events.subscribers.retain(|subscriber| subscriber.send(json.clone()).is_ok());
        }
    }

    /// Whether the app follows the notebooks' state, so it can ask the user.
    pub fn followed(&self) -> bool {
        !self.events.lock().unwrap().subscribers.is_empty()
    }

    /// A new `/events` subscriber: the state now, then each change.
    fn subscribe(&self) -> Result<(String, Receiver<String>), String> {
        let _publishing = self.publishing.lock().unwrap();
        let first = self.compose()?;
        let (tx, rx) = mpsc::channel();
        self.events.lock().unwrap().subscribers.push(tx);
        Ok((first, rx))
    }

    /// Serve `GET /events`: the notebooks' state now and after every change,
    /// until the client goes.
    pub fn stream_events(&self, request: &Head, mut client: TcpStream) -> io::Result<()> {
        let (first, rx) = match self.subscribe() {
            Ok(subscription) => subscription,
            Err(e) => {
                let body = json!({ "error": e }).to_string();
                return http::respond(&mut client, "502 Bad Gateway", Some("application/json"), body.as_bytes(), false);
            }
        };
        let chunked = request.keeps_alive();
        write!(
            client,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n{}\r\n",
            if chunked { "Transfer-Encoding: chunked\r\n" } else { "Connection: close\r\n" }
        )?;
        for json in std::iter::once(first).chain(rx) {
            let event = format!("data: {json}\n\n");
            if chunked {
                write!(client, "{:x}\r\n{event}\r\n", event.len())?;
            } else {
                client.write_all(event.as_bytes())?;
            }
            client.flush()?;
        }
        Ok(())
    }

    /// The time by the core's clock (Unix seconds).
    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    /// A tool call names the notebook it works on: that counts as activity.
    pub fn note_activity(&self, arguments: &Value) {
        let Some(id) = arguments.get("notebook_id").map(julia_string).and_then(|id| parse_uuid(&id)) else { return };
        let now = (self.clock)();
        if let Some(notebook) = self.state.lock().unwrap().notebooks.get_mut(&id) {
            notebook.last_active = Some(now);
        }
    }

    /// `keep_notebook_alive`: exempt a notebook from idle stop, or stop exempting it.
    pub fn keep_alive(&self, arguments: &Value) -> Result<Value, String> {
        let id = self.notebook_arg(arguments.get("notebook_id").unwrap_or(&Value::String(String::new())))?;
        let keep = arguments.get("keep").and_then(Value::as_bool).ok_or("ArgumentError: invalid_keep::keep must be true or false")?;
        let now = (self.clock)();
        self.with_state(&id, |notebook| {
            notebook.kept_alive = keep;
            notebook.last_active = Some(now);
        });
        Ok(json!({ "notebook_id": id, "kept_alive": keep }))
    }

    /// An open notebook's id from a tool argument, refused as Julia's UUID
    /// parsing and lookup refused it.
    fn notebook_arg(&self, value: &Value) -> Result<String, String> {
        let shown = julia_string(value);
        let id = uuid_value(value).ok_or_else(|| format!("ArgumentError: invalid_notebook_id::Invalid notebook ID: '{shown}'"))?;
        let known = self.state.lock().unwrap().notebooks.contains_key(&id);
        if known || self.snapshot(&id).is_ok() {
            return Ok(id);
        }
        Err(tools::notebook_not_found(&shown))
    }

    /// The app binds a session to its notebook; an empty path clears it.
    pub fn bind(&self, owner: &str, path: &str) {
        let now = self.now();
        let mut state = self.state.lock().unwrap();
        if path.is_empty() {
            state.bindings.remove(owner);
        } else {
            state.bindings.insert(owner.to_owned(), canonical_path(path).unwrap_or_else(|_| path.to_owned()));
            state.record_bound(owner, now);
        }
    }

    /// A session makes a tool call.
    /// Notebook `id`'s code as it is now, cells in order, as a number that
    /// changes when any cell's code or the order does. None for an id that
    /// names no open notebook.
    pub fn code_print(&self, id: &str) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let nb = self.snapshot(id).ok()?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for cell in &nb.order {
            (cell, nb.cells.get(cell).map(|c| &c.code)).hash(&mut hasher);
        }
        Some(hasher.finish())
    }

    pub fn note_call(&self, owner: &str) {
        if owner.is_empty() {
            return;
        }
        let now = self.now();
        let mut state = self.state.lock().unwrap();
        state.record(owner, now).last_call = Some(now);
    }

    /// The sessions other than `owner` that work in a notebook that is open and made a
    /// tool call in the last `within` seconds: how many, and how long ago the latest did.
    pub fn recent_sessions(&self, owner: &str, within: f64) -> Result<Value, String> {
        let open: Vec<String> = self.snapshots()?.into_iter().map(|nb| canonical_path(&nb.path).unwrap_or(nb.path)).collect();
        let now = self.now();
        let mut state = self.state.lock().unwrap();
        state.forget_old(now);
        let ago: Vec<f64> = (state.bindings.iter())
            .filter(|(session, path)| session.as_str() != owner && open.contains(path))
            .filter_map(|(session, _)| state.seen.get(session)?.last_call)
            .map(|last| (now - last).max(0.0))
            .filter(|ago| *ago <= within)
            .collect();
        Ok(json!({ "count": ago.len(), "active_seconds_ago": ago.iter().copied().reduce(f64::min).map(|ago| ago as u64) }))
    }

    pub fn bound(&self, owner: &str) -> Option<String> {
        self.state.lock().unwrap().bindings.get(owner).cloned()
    }

    /// After a successful open or new: bind the session if it isn't bound yet.
    fn opened_by(&self, owner: &str, path: &str) {
        if !owner.is_empty() {
            let path = canonical_path(path).unwrap_or_else(|_| path.to_owned());
            let now = self.now();
            let mut state = self.state.lock().unwrap();
            state.bindings.entry(owner.to_owned()).or_insert(path);
            state.record_bound(owner, now);
        }
    }

    /// Drop the session's binding to `path`, a notebook that is no longer open.
    fn unbind(&self, owner: &str, path: &str) {
        let mut state = self.state.lock().unwrap();
        if state.bindings.get(owner).is_some_and(|bound| bound == path) {
            state.bindings.remove(owner);
        }
    }

    /// Why a session may not make this call: it works on another notebook.
    /// Other notebooks stay readable. A notebook that is no longer open is not one it works on.
    pub fn refusal(&self, owner: &str, tool: &str, arguments: &Value, folder: &Folder) -> Option<String> {
        if owner.is_empty() {
            return None;
        }
        let bound = self.bound(owner)?;
        let refuse = |what: &str| {
            // An engine that can't say leaves the binding.
            if self.snapshots().is_ok_and(|open| !open.iter().any(|nb| canonical_path(&nb.path).is_ok_and(|p| p == bound))) {
                self.unbind(owner, &bound);
                return None;
            }
            Some(format!(
                "ArgumentError: one_notebook::This session works on one notebook, {bound}, so it can't {what}. \
                 You can still read other notebooks as plain .jl files. \
                 To work on another notebook, suggest the user start a new session with it."
            ))
        };
        if tool == "open_notebook" || tool == "new_notebook" {
            let requested = arguments.get("path").and_then(Value::as_str);
            if let Some(requested) = requested {
                match requested_path(requested, folder).and_then(|path| canonical_path(&path)) {
                    Ok(path) if path == bound => return None,
                    Err(error) => return Some(error),
                    Ok(_) => {}
                }
            }
            return match requested {
                Some(requested) => refuse(&format!("{} {requested}", if tool == "open_notebook" { "open" } else { "create" })),
                None => refuse("create another notebook"),
            };
        }
        if !WRITE_TOOLS.contains(&tool) || tool == "run_shell" {
            return None;
        }
        let id = parse_uuid(&julia_string(arguments.get("notebook_id").unwrap_or(&Value::String(String::new()))))?;
        let known = self.state.lock().unwrap().notebooks.get(&id).map(|nb| nb.path.clone()).filter(|p| !p.is_empty());
        let path = known.or_else(|| self.snapshot(&id).ok().map(|nb| nb.path))?;
        match canonical_path(&path) {
            Ok(canonical) if canonical == bound => None,
            Err(error) => Some(error),
            Ok(_) => refuse(&format!("change or run {path}")),
        }
    }

    /// `endeavor/set_idle_limit`: stop notebooks idle this long; 0 never stops them.
    pub fn set_idle_limit(&self, hours: f64) {
        self.state.lock().unwrap().idle_limit_hours = hours;
    }

    pub fn idle_limit_hours(&self) -> f64 {
        self.state.lock().unwrap().idle_limit_hours
    }

    /// How many notebooks the engine has open, if it answers.
    pub fn open_count(&self) -> Option<usize> {
        self.snapshots().ok().map(|snapshots| snapshots.len())
    }

    /// Stop every notebook idle longer than the limit (no tool call on it, no
    /// save or run through the engine, no cells running), except kept-alive
    /// ones. The stopped paths.
    pub fn stop_idle(&self) -> Vec<String> {
        let hours = self.state.lock().unwrap().idle_limit_hours;
        if hours <= 0.0 {
            return Vec::new();
        }
        let Ok(snapshots) = self.snapshots() else { return Vec::new() };
        let now = (self.clock)();
        let idle: Vec<Snapshot> = {
            let mut state = self.state.lock().unwrap();
            snapshots
                .into_iter()
                .filter(|nb| {
                    let notebook = state.notebooks.entry(nb.id.clone()).or_default();
                    if notebook.kept_alive {
                        return false;
                    }
                    if nb.cells.values().any(|c| c.running || c.queued) {
                        notebook.last_active = Some(now);
                        return false;
                    }
                    now - *notebook.last_active.get_or_insert(now) >= hours * 3600.0
                })
                .collect()
        };
        let whole_hours = hours.round_ties_even() as i64;
        for nb in &idle {
            // Recorded first, so the event after the stop carries it.
            let key = canonical_path(&nb.path).unwrap_or_else(|_| nb.path.clone());
            let entry = json!({ "path": nb.path, "hours": whole_hours, "safe_preview": nb.safe_preview });
            {
                let mut state = self.state.lock().unwrap();
                state.idle_stopped.retain(|(p, _)| *p != key);
                state.idle_stopped.push((key, entry));
            }
            if self.shutdown(&nb.id).is_ok() {
                self.publish();
            }
            eprintln!("[ Info: Stopped {} after {whole_hours} hours idle", nb.path);
        }
        idle.into_iter().map(|nb| nb.path).collect()
    }

    fn shutdown(&self, id: &str) -> Result<bool, String> {
        let result = self.call("shutdown", json!({ "notebook_id": id }))?;
        self.state.lock().unwrap().notebooks.remove(id);
        Ok(result["safe_preview"].as_bool().unwrap_or(false))
    }

    /// `endeavor/stop_notebook`: shut down the open notebook at `path` (the
    /// app's "Stop notebook"). `safe_preview` says whether it was still in safe
    /// preview, so the app can reopen it the same way.
    pub fn stop_notebook(&self, path: &str) -> Result<Value, String> {
        let target = canonical_path(path)?;
        let snapshots = self.snapshots()?;
        let Some(nb) = snapshots.iter().find(|nb| canonical_path(&nb.path).is_ok_and(|p| p == target)) else {
            return Ok(json!({ "stopped": false }));
        };
        let safe_preview = self.shutdown(&nb.id)?;
        self.publish();
        Ok(json!({ "stopped": true, "safe_preview": safe_preview }))
    }

    /// The user's run reached `cells` (each with its `last_run` from before
    /// that run) while a call asking to run them waited, and the user then
    /// allowed it (`endeavor/answer_run`'s `user_ran`).
    pub fn run_anyway(&self, notebook_id: &str, cells: &Value) -> Result<Value, String> {
        let cells: Vec<(String, f64)> = cells
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| Some((c["cell_id"].as_str()?.to_owned(), c["last_run"].as_f64().unwrap_or(0.0))))
            .collect();
        let now = (self.clock)();
        self.with_state(notebook_id, |state| {
            for (cell, last_run) in cells {
                state.user_runs.insert(cell, (last_run, now));
            }
        });
        Ok(json!({}))
    }

    /// `endeavor/restart_notebook`: Pluto's own Restart, a new process and then
    /// every cell runs; refused in safe preview. The notebook stays open with
    /// the same cells, so the core keeps all it knows of it; its pending cells
    /// stop being pending as they run.
    pub fn restart(&self, notebook_id: &str) -> Result<Value, String> {
        let result = self.call("restart", json!({ "notebook_id": notebook_id, "timeout": tools::TIMEOUT_SECONDS }))?;
        self.publish();
        Ok(result)
    }

    /// `endeavor/move_notebook`: rename or move an open notebook's file (the
    /// app's Rename… and Move to…), never over another file. Sessions bound to
    /// it, and an `idle_stopped` entry for it, follow it to its new path.
    pub fn move_notebook(&self, notebook_id: &str, path: &str) -> Result<Value, String> {
        let nb = self.snapshot(notebook_id)?;
        let target = absolute_path(path)?;
        // A notebook keeps its engine: a Julia notebook's name ends in .jl, an R notebook's in .R.
        let (fits, extension) = match self.backend_of(&nb.id) {
            Backend::Pluto => (target.ends_with(".jl"), ".jl"),
            Backend::Ember => (engines::of_path(&target) == Backend::Ember, ".R"),
        };
        if !fits {
            return Err(format!("ArgumentError: invalid_path::Notebook path must end in {extension}: '{target}'"));
        }
        if std::path::Path::new(&target).exists() {
            return Err(format!("ArgumentError: file_exists::'{target}' already exists"));
        }
        let dir = parent_dir(&target);
        if !std::path::Path::new(&dir).is_dir() {
            return Err(format!("ArgumentError: invalid_path::Directory does not exist: '{dir}'"));
        }
        let old = canonical_path(&nb.path).unwrap_or_else(|_| nb.path.clone());
        let result = self.call("move", json!({ "notebook_id": nb.id, "path": target }))?;
        let moved = result["path"].as_str().unwrap_or(&target).to_owned();
        let new = canonical_path(&moved).unwrap_or_else(|_| moved.clone());
        {
            let mut state = self.state.lock().unwrap();
            for bound in state.bindings.values_mut().filter(|bound| **bound == old) {
                *bound = new.clone();
            }
            for (key, entry) in state.idle_stopped.iter_mut().filter(|(key, _)| *key == old) {
                *key = new.clone();
                entry["path"] = moved.clone().into();
            }
            if let Some(notebook) = state.notebooks.get_mut(&nb.id) {
                notebook.path = moved;
            }
        }
        self.publish();
        Ok(result)
    }

    /// `endeavor/new_notebook`: the app's "New notebook" for session `owner`,
    /// a new notebook in its folder that becomes its notebook.
    pub fn new_for(&self, owner: &str, folder: &Folder) -> Result<Value, String> {
        self.bind(owner, "");
        let Reply::Json(result) = self.tool(owner, "new_notebook", &json!({}), folder, Instant::now())? else {
            unreachable!("new_notebook answers JSON")
        };
        self.publish();
        Ok(result)
    }

    #[cfg(test)]
    fn idle_stopped(&self) -> Vec<Value> {
        self.state.lock().unwrap().idle_stopped.iter().map(|(_, entry)| entry.clone()).collect()
    }
}

/// `endeavor/file_info`: whether a file is at `path` on this machine, and when
/// it last changed (Unix seconds, as Julia's `mtime` gives them).
pub fn file_info(path: &str) -> Result<Value, String> {
    match std::fs::metadata(absolute_path(path)?) {
        Ok(meta) if meta.is_file() => Ok(json!({ "exists": true, "modified": crate::host_tools::mtime(&meta) })),
        _ => Ok(json!({ "exists": false })),
    }
}

/// FNV-1a: the app only compares versions with each other.
/// Of the code as the engine keeps it: Ember drops trailing blank lines and turns CRLF into LF.
fn hash(code: &str) -> u64 {
    code.trim_end().bytes().filter(|&b| b != b'\r').fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// A UUID in its only form Julia parses (36 characters, hex and hyphens), lowercased.
fn parse_uuid(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let hyphen = |i: usize| matches!(i, 8 | 13 | 18 | 23);
    let valid = bytes.len() == 36 && bytes.iter().enumerate().all(|(i, b)| if hyphen(i) { *b == b'-' } else { b.is_ascii_hexdigit() });
    valid.then(|| text.to_ascii_lowercase())
}

/// A JSON value as Julia's `UUID(value)` takes it: a UUID string, or a
/// whole number (a Bool is 0 or 1).
fn uuid_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => parse_uuid(s),
        Value::Bool(b) => Some(uuid_of(*b as u128)),
        Value::Number(n) => match (n.as_u64(), n.as_f64()) {
            (Some(n), _) => Some(uuid_of(n as u128)),
            (None, Some(x)) if x >= 0.0 && x.fract() == 0.0 && x < 2f64.powi(128) => Some(uuid_of(x as u128)),
            _ => None,
        },
        _ => None,
    }
}

fn uuid_of(value: u128) -> String {
    let hex = format!("{value:032x}");
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// A notebook path as Julia's runtime compared them: absolute, `~` expanded,
/// and resolved through symlinks as far as it exists.
#[cfg(unix)]
pub fn canonical_path(path: &str) -> Result<String, String> {
    let absolute = absolute_path(path)?;
    let real = |p: &str| std::fs::canonicalize(p).ok().map(|p| p.display().to_string());
    if let Some(real) = real(&absolute) {
        return Ok(real);
    }
    let split = absolute.rfind('/').unwrap_or(0);
    let (dir, base) = (if split == 0 { "/" } else { &absolute[..split] }, &absolute[split + 1..]);
    match real(dir).filter(|_| std::path::Path::new(dir).is_dir()) {
        Some(dir) if dir.ends_with('/') => Ok(format!("{dir}{base}")),
        Some(dir) => Ok(format!("{dir}/{base}")),
        None => Ok(absolute),
    }
}

#[cfg(windows)]
pub fn canonical_path(path: &str) -> Result<String, String> {
    let absolute = std::path::PathBuf::from(absolute_path(path)?);
    if let Ok(real) = wire::files::real_path(&absolute) {
        return Ok(real.display().to_string());
    }
    let real_dir = absolute.parent().filter(|dir| dir.is_dir()).and_then(|dir| wire::files::real_path(dir).ok());
    match (real_dir, absolute.file_name()) {
        (Some(dir), Some(base)) => Ok(dir.join(base).display().to_string()),
        _ => Ok(absolute.display().to_string()),
    }
}

/// Where a session's relative paths start.
#[derive(Clone, Debug, PartialEq)]
pub enum Folder {
    /// The session's working folder.
    In(String),
    /// None was given (the app's session): the runtime process's own working folder.
    Process,
    /// The session was not told its project folder, so a relative path means nothing.
    Unknown,
}

impl Folder {
    /// The folder, when there is one.
    pub fn dir(&self) -> Option<&str> {
        match self {
            Folder::In(dir) => Some(dir),
            _ => None,
        }
    }
}

/// What a session without a folder is told when it gives a path that needs one.
fn folder_unknown() -> String {
    tools::argument_error("invalid_path::Give an absolute path: this server was not told the project folder.")
}

/// A path an agent gave, as the tools resolve it: `~` expanded, and a relative
/// path taken from the session's `folder`. The one place a relative path is resolved.
fn requested_path(path: &str, folder: &Folder) -> Result<String, String> {
    let expanded = expand_user(path)?;
    match folder {
        Folder::Unknown if is_fully_absolute(&expanded) => Ok(expanded),
        Folder::Unknown => Err(folder_unknown()),
        _ if is_absolute(&expanded) => Ok(expanded),
        Folder::In(folder) => absolute_path(&format!("{folder}/{expanded}")),
        Folder::Process => Ok(expanded),
    }
}

/// Why a session with no folder may not make this call: it gives a relative path, or none to
/// `new_notebook`, which would put the notebook in a folder it does not know. Nothing else is refused.
/// With the error, whether it already says what to do (so it needs no pointer to the guide).
pub(crate) fn path_refusal(tool: &str, arguments: &Value, folder: &Folder) -> Option<(String, bool)> {
    if *folder != Folder::Unknown {
        return None;
    }
    match (tool, arguments.get("path").filter(|path| !path.is_null())) {
        ("open_notebook" | "new_notebook", Some(Value::String(path))) => match requested_path(path, folder) {
            Ok(_) => None,
            Err(error) if error == folder_unknown() => Some((error, true)),
            Err(error) => Some((format!("ArgumentError: invalid_path::{}", error.trim_start_matches("ArgumentError: ")), false)),
        },
        ("new_notebook", None) => Some((folder_unknown(), true)),
        _ => None,
    }
}

/// Julia's `expanduser`, which leaves paths alone on Windows.
fn expand_user(path: &str) -> Result<String, String> {
    if cfg!(windows) {
        return Ok(path.to_owned());
    }
    match path.strip_prefix('~') {
        None => Ok(path.to_owned()),
        Some("") => Ok(home()),
        Some(rest) if rest.starts_with('/') => Ok(format!("{}{rest}", home())),
        Some(_) => Err("ArgumentError: ~user tilde expansion not yet implemented".into()),
    }
}

/// Julia's `abspath(expanduser(path))`.
#[cfg(unix)]
fn absolute_path(path: &str) -> Result<String, String> {
    let expanded = expand_user(path)?;
    if expanded.starts_with('/') {
        return Ok(normpath(&expanded));
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    Ok(normpath(&format!("{}/{expanded}", cwd.display())))
}

#[cfg(windows)]
fn absolute_path(path: &str) -> Result<String, String> {
    std::path::absolute(expand_user(path)?).map(|p| p.display().to_string()).map_err(|e| e.to_string())
}

/// Julia's `isabspath`: on Windows, `\x`, `C:\x` and `C:/x`.
fn is_absolute(path: &str) -> bool {
    if cfg!(windows) {
        let drive = path.find(':').filter(|&colon| colon > 0 && path[..colon].bytes().all(|b| b.is_ascii_alphabetic())).map_or(0, |colon| colon + 1);
        return path[drive..].starts_with(['/', '\\']);
    }
    path.starts_with('/')
}

/// An absolute path that names its own root: on Windows, with a drive or as `\\server\share`; `\x` and
/// `/x` are rooted on the working folder's drive, which a session with no folder does not know.
fn is_fully_absolute(path: &str) -> bool {
    if cfg!(windows) {
        let drive = path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':';
        return (drive && path[2..].starts_with(['/', '\\'])) || path.starts_with("\\\\") || path.starts_with("//");
    }
    path.starts_with('/')
}

/// The folder an absolute, normalized path is in.
fn parent_dir(path: &str) -> String {
    #[cfg(windows)]
    if let Some(dir) = std::path::Path::new(path).parent().filter(|dir| !dir.as_os_str().is_empty()) {
        return dir.display().to_string();
    }
    match &path[..path.rfind('/').unwrap_or(0)] {
        "" => "/".into(),
        dir => dir.into(),
    }
}

#[cfg(test)]
mod tests;
