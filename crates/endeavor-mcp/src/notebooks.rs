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

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
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
pub use tools::{Reply, WAIT_FLOOR_SECONDS, WAIT_SECONDS};
pub(crate) use tools::{screenshot_result, snapshot_may_help};

const IDLE_CHECK: Duration = Duration::from_secs(300);
/// How long the result of an open that outlasted its call is kept for the same call made again.
const OPENING_KEPT: Duration = Duration::from_secs(600);

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
    /// Why the engine ended, once it stopped answering: its process's exit and last error, if known.
    fn why_ended(&self) -> Option<String> {
        None
    }
}

/// Starts an engine's adapter: what answers for it once it's up, or why it couldn't start. The
/// call came at the `Instant` and waits up to the `Duration` from then, however long it queued
/// behind other starts (#175); Julia's start waits no longer than that.
pub type Starter = Box<dyn Fn(Backend, Instant, Duration) -> Result<Arc<dyn Upstream>, String> + Send + Sync>;

/// Julia's bridge, once it answers.
pub struct Julia {
    pub port: OnceLock<u16>,
    /// The engines' key the core gave Julia, not the MCP bearer token.
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
    /// The runs sessions started that haven't ended (`run_finished`), each with its cells that
    /// hadn't finished when the tool answered: what that session's Stop interrupts.
    started: Vec<Started>,
    /// Runs whose `run_finished` came before the tool that started them recorded them (a quick
    /// run), so they aren't recorded after; the latest `ENDED_KEPT`.
    ended: VecDeque<String>,
}

/// A run a session started (`NotebookState::started`). `run` is the engine's id for it; an
/// engine that gives none says only which cells finished, and its cells count until then.
struct Started {
    owner: String,
    run: Option<String>,
    cells: HashSet<String>,
}

const ENDED_KEPT: usize = 64;

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

/// A notebook's open or creation in its engine, which goes on after the call that asked for it
/// stopped waiting: what it was asked with, and its result and when it came, once it has.
struct Opening {
    params: Value,
    done: Mutex<Option<(Result<Value, String>, Instant)>>,
    came: Condvar,
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
    /// What an engine's start has to tell the agent: warnings for the next notebook opened or made.
    notices: Mutex<Vec<String>>,
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
    /// Each agent session's notebook kind, when its client said it (`endeavor/set_session_folder`'s
    /// `kind`): the engine of a `new_notebook` without a path. Pluto's for a session with none.
    kinds: Mutex<HashMap<String, Backend>>,
    /// Opens and creations under way or not yet collected, by what they open (`open_waited`).
    openings: Mutex<HashMap<String, Arc<Opening>>>,
    /// Agents' calls on a notebook under way (`running`): owner, JSON-RPC id, notebook, and
    /// whether the call runs code. The agent's cancellation of one is its Stop (`cancelled`).
    runs: Mutex<Vec<(String, Value, String, bool)>>,
}

/// An agent's call on a notebook, noted until it drops (`Notebooks::running`).
pub struct Running<'a> {
    notebooks: &'a Notebooks,
    owner: String,
    request: Value,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut runs = self.notebooks.runs.lock().unwrap();
        if let Some(at) = runs.iter().position(|(owner, request, _, _)| *owner == self.owner && *request == self.request) {
            runs.remove(at);
        }
    }
}

/// One call to the engines' adapter (`message`): its result, or the error it raised.
fn adapter_call(engines: &Engines, message: &Value) -> Result<Value, String> {
    let reply = engines.adapter(message.to_string().as_bytes()).map_err(|e| e.to_string())?;
    let mut reply: Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
    match reply.get("error") {
        Some(error) => Err(julia_string(error)),
        None => Ok(reply["result"].take()),
    }
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
    /// Ember's lazy mode: a run marks the cells that depend on it stale
    /// instead of rerunning them (#196). Pluto has no such mode.
    lazy: bool,
    /// An R notebook's snapshot date, which its packages' versions come from (#201). Pluto has none.
    package_date: Option<String>,
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
            lazy: flag(&value["lazy"]),
            package_date: value["package_date"].as_str().map(str::to_owned),
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
        if self.lazy {
            summary["on_cell_change"] = json!("lazy");
        }
        if let Some(date) = &self.package_date {
            summary["package_date"] = json!(date);
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
        Graph { cells: cells.collect(), order: names(&value["order"]) }
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
            notices: Mutex::default(),
            clock,
            state: Mutex::new(State { notebooks: HashMap::new(), seq: 0, idle_limit_hours: IDLE_HOURS, idle_stopped: Vec::new(), bindings: HashMap::new(), seen: HashMap::new() }),
            publishing: Mutex::default(),
            events: Mutex::default(),
            build: OnceLock::new(),
            exits_when_idle: false,
            kinds: Mutex::default(),
            openings: Mutex::default(),
            runs: Mutex::default(),
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

    /// Say `warning` with the next notebook opened or made.
    pub fn warn_next(&self, warning: String) {
        self.notices.lock().unwrap().push(warning);
    }

    /// What to say with this notebook opened or made, said once.
    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap())
    }

    /// The engine notebook `id` is open in.
    pub fn backend_of(&self, id: &str) -> Backend {
        self.engines.backend_of(id)
    }

    /// Start `backend`'s engine unless it runs. Julia keeps its own start in order (core.rs), and can
    /// take minutes; the other engines' starts are taken one at a time here.
    fn start_engine(&self, backend: Backend, wait: Duration) -> Result<(), String> {
        if self.engines.has(backend) {
            return Ok(());
        }
        let began = Instant::now();
        let _starting = (backend != Backend::Pluto).then(|| self.starting.lock().unwrap());
        if self.engines.has(backend) {
            return Ok(());
        }
        let Some(starter) = self.starter.get() else { return Ok(()) };
        let upstream = starter(backend, began, wait)?;
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
                    self.engines.reopened(|closed| canonical_path(closed).is_ok_and(|closed| closed == path));
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
            // A run the tools didn't wait for ended: the cells it finished are no longer
            // pending, and none of its cells is a session's run that its Stop interrupts, even
            // one that a later run (a restart's) queued again before it ended.
            "run_finished" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    let finished: Vec<&str> = params["cells"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                    for cell in &finished {
                        notebook.pending.remove(*cell);
                    }
                    match params["run"].as_str() {
                        Some(run) => {
                            let before = notebook.started.len();
                            notebook.started.retain(|started| started.run.as_deref() != Some(run));
                            if notebook.started.len() == before {
                                notebook.ended.push_back(run.to_owned());
                                if notebook.ended.len() > ENDED_KEPT {
                                    notebook.ended.pop_front();
                                }
                            }
                        }
                        None => {
                            for started in &mut notebook.started {
                                started.cells.retain(|cell| !finished.contains(&cell.as_str()));
                            }
                            notebook.started.retain(|started| started.run.is_some() || !started.cells.is_empty());
                        }
                    }
                }
                true
            }
            // Its process ended: no run started before goes on.
            "process_exited" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    notebook.started.clear();
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
            "topology_changed" | "resync" => true,
            _ => false,
        }
    }

    /// One call to the engine's adapter: its result, or the error it raised.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        adapter_call(&self.engines, &json!({ "method": method, "params": params }))
    }

    /// Whether an open or creation for `key` is under way or kept (`open_waited`).
    fn opening(&self, key: &str) -> bool {
        self.openings.lock().unwrap().get(key).is_some_and(|opening| opening.done.lock().unwrap().as_ref().is_none_or(|(_, at)| at.elapsed() < OPENING_KEPT))
    }

    /// The engine's `open` or `new` (`method`), starting the engine first, waited for up to `wait`,
    /// with the params it was made with. The first notebook in a new Julia process compiles for a
    /// while, so with Julia's start the call could outlast what an agent's call waits. Then the open
    /// goes on, and the call answers `notebook_opening` (`doing` says what); the same call again, with
    /// the same `key`, waits for that open and gets its result and its params (an earlier call's,
    /// which may differ from this one's), kept for `OPENING_KEPT`. If no call comes again, the open
    /// still ends in the engine, but no session is recorded as having opened it; a later
    /// `open_notebook` joins it as already open.
    fn open_waited(&self, method: &str, params: Value, key: String, wait: Duration, doing: &str) -> Result<(Value, Value), String> {
        let began = Instant::now();
        self.start_engine(engines::of_params(&params), wait)?;
        let why = "the first notebook in a new Julia process takes a while to compile";
        self.waited(method, params, key, (began + wait).saturating_duration_since(Instant::now()), &format!("{doing} is taking longer than one call waits: {why}"))
    }

    /// The engine's `method` on a background thread, waited for up to `wait`, with the params it
    /// was made with (`open_waited`). When the wait runs out, the call answers `notebook_opening`
    /// with `taking` and goes on; the same call again, with the same `key`, waits for it and gets
    /// its result and its params (an earlier call's, which may differ from this one's), kept for
    /// `OPENING_KEPT`.
    fn waited(&self, method: &str, params: Value, key: String, wait: Duration, taking: &str) -> Result<(Value, Value), String> {
        let began = Instant::now();
        let opening = {
            let mut openings = self.openings.lock().unwrap();
            openings.retain(|_, opening| opening.done.lock().unwrap().as_ref().is_none_or(|(_, at)| at.elapsed() < OPENING_KEPT));
            let opening = openings.entry(key.clone()).or_insert_with(|| {
                let opening = Arc::new(Opening { params: params.clone(), done: Mutex::default(), came: Condvar::new() });
                let (engines, message, done) = (self.engines.clone(), json!({ "method": method, "params": params }), opening.clone());
                std::thread::spawn(move || {
                    let result = adapter_call(&engines, &message);
                    *done.done.lock().unwrap() = Some((result, Instant::now()));
                    done.came.notify_all();
                });
                opening
            });
            opening.clone()
        };
        let deadline = began + wait;
        let mut done = opening.done.lock().unwrap();
        loop {
            if let Some((result, _)) = done.as_ref() {
                let result = result.clone();
                drop(done);
                let mut openings = self.openings.lock().unwrap();
                if openings.get(&key).is_some_and(|kept| Arc::ptr_eq(kept, &opening)) {
                    openings.remove(&key);
                }
                return result.map(|result| (result, opening.params.clone()));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("notebook_opening::{taking}. It goes on by itself. Make the same call again to wait for it."));
            }
            done = opening.came.wait_timeout(done, left).unwrap().0;
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
    ///    "engine_ended": [{path, language, why}, ...]: notebooks that closed because their
    ///      engine (R's) ended, until each is opened again,
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
        let engine_ended: Vec<Value> = self.engines.closed().iter().map(|c| json!({ "path": c.path, "language": c.language, "why": c.why })).collect();
        let mut event = json!({ "notebooks": list, "cells": cells, "idle_stopped": idle_stopped, "engine_ended": engine_ended, "asks": self.asks.list() });
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
    pub fn stream_events(&self, request: &Head, mut client: wire::Shared) -> io::Result<()> {
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

    /// Session `owner`'s notebook kind: the engine its new notebooks without a path are made in.
    /// None forgets it, and they are Pluto's again.
    pub fn set_kind(&self, owner: &str, kind: Option<Backend>) {
        let mut kinds = self.kinds.lock().unwrap();
        match kind {
            Some(kind) => kinds.insert(owner.to_owned(), kind),
            None => kinds.remove(owner),
        };
    }

    /// Session `owner`'s notebook kind, if its client said it.
    pub fn kind(&self, owner: &str) -> Option<Backend> {
        self.kinds.lock().unwrap().get(owner).copied()
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
                 You can still read other notebooks as plain files. \
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
    /// preview, so the app can reopen it the same way. A notebook at `path` that
    /// closed when its engine ended leaves `engine_ended`.
    pub fn stop_notebook(&self, path: &str) -> Result<Value, String> {
        let target = canonical_path(path)?;
        let snapshots = self.snapshots()?;
        let Some(nb) = snapshots.iter().find(|nb| canonical_path(&nb.path).is_ok_and(|p| p == target)) else {
            // One that closed when its engine ended: the user lets it go.
            if self.engines.reopened(|closed| engines::same_path(closed, &target)) {
                self.publish();
                return Ok(json!({ "stopped": true, "safe_preview": false }));
            }
            return Ok(json!({ "stopped": false }));
        };
        let safe_preview = self.shutdown(&nb.id)?;
        self.publish();
        Ok(json!({ "stopped": true, "safe_preview": safe_preview }))
    }

    /// Note `owner`'s call `request` on `notebook_id` until the result drops, so that cancelling
    /// it can interrupt the notebook (`cancelled`). `runs_code`: the call runs code itself.
    pub fn running(&self, owner: &str, request: &Value, notebook_id: &str, runs_code: bool) -> Running<'_> {
        self.runs.lock().unwrap().push((owner.to_owned(), request.clone(), notebook_id.to_owned(), runs_code));
        Running { notebooks: self, owner: owner.to_owned(), request: request.clone() }
    }

    /// The notebook of `owner`'s call `request`, if it is under way (`running`), and whether the
    /// call runs code: the agent cancelled it, which is the user's Stop.
    pub fn cancelled(&self, owner: &str, request: &Value) -> Option<(String, bool)> {
        let runs = self.runs.lock().unwrap();
        runs.iter().find(|(o, r, _, _)| o == owner && r == request).map(|(_, _, notebook, runs_code)| (notebook.clone(), *runs_code))
    }

    /// The user's Stop, for `owner`'s cancelled call on `notebook_id`: interrupt the notebook's
    /// run, as Pluto's own Stop does, if the call was running code, or if a run this session
    /// started there is still going (an agent often starts a run without waiting, then waits by
    /// reading the cell). A run only the user or another session started is left alone.
    pub fn stop_for(&self, owner: &str, notebook_id: &str, runs_code: bool) {
        if !runs_code {
            if !self.with_state(notebook_id, |state| state.started.iter().any(|started| started.owner == owner)) {
                return;
            }
            let Ok(nb) = self.snapshot(notebook_id) else { return };
            if !self.own_run_going(owner, &nb) {
                return;
            }
        }
        self.interrupt(notebook_id, owner);
    }

    /// Whether a run `owner` started in `nb` is still going, as `nb` shows it. A run the engine
    /// named goes on, with the cells that depend on its cells, until its `run_finished`, and
    /// while it does some cell is running or queued: a notebook with none has seen it end. A
    /// run the engine didn't name goes on while one of its own cells hasn't finished. A run
    /// seen to have ended is forgotten.
    fn own_run_going(&self, owner: &str, nb: &Snapshot) -> bool {
        let busy = nb.cells.values().any(|c| c.running || c.queued);
        self.with_state(&nb.id, |state| {
            for started in state.started.iter_mut().filter(|started| started.owner == owner) {
                if started.run.is_none() || !busy {
                    started.cells.retain(|id| busy && nb.cells.get(id).is_some_and(|c| c.running || c.queued));
                }
            }
            state.started.retain(|started| (started.run.is_some() && busy) || !started.cells.is_empty());
            state.started.iter().any(|started| started.owner == owner)
        })
    }

    /// The notebooks where runs `owner` started may still be going (`stop_for` checks): what the
    /// app's Stop for that session looks at (`endeavor/stop_session`).
    pub fn session_runs(&self, owner: &str) -> Vec<String> {
        if owner.is_empty() {
            return Vec::new();
        }
        let state = self.state.lock().unwrap();
        let mut ids: Vec<String> = state.notebooks.iter().filter(|(_, nb)| nb.started.iter().any(|started| started.owner == owner)).map(|(id, _)| id.clone()).collect();
        ids.sort();
        ids
    }

    /// Interrupt `notebook_id`'s run, as Pluto's own Stop does, for `owner`'s cancelled call: the
    /// cells end interrupted. A cell that doesn't stop (one long `sleep`) ends the notebook's
    /// process, as with Pluto's Stop, and the notebook waits for a restart.
    pub fn interrupt(&self, notebook_id: &str, owner: &str) {
        match self.call("interrupt", json!({ "notebook_id": notebook_id })) {
            Ok(reply) if reply["interrupted"] == false => eprintln!("[ Info: Session {owner} stopped: nothing was running in notebook {notebook_id}"),
            Ok(_) => eprintln!("[ Info: Interrupted notebook {notebook_id}: session {owner} stopped"),
            Err(error) => eprintln!("┌ Warning: Couldn't interrupt notebook {notebook_id} for session {owner}'s Stop: {error}"),
        }
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
        // The runs sessions started end with the old process; the restart's run is no session's.
        self.with_state(notebook_id, |state| state.started.clear());
        let result = self.call("restart", json!({ "notebook_id": notebook_id, "timeout": tools::TIMEOUT_SECONDS }))?;
        self.publish();
        Ok(result)
    }

    /// `endeavor/restore_cells`: the app's Restore on a turn's changed cells,
    /// the user putting back what an agent's turn changed. `cells` is a list
    /// of {cell_id, expected, code}: `expected` the code the turn left the
    /// cell with, `code` the code to put back, or null to delete a cell the
    /// turn added. All or nothing: if any cell's code isn't `expected` (or
    /// the cell is gone), nothing changes and `changed` names those cells.
    /// Nothing runs, except Pluto's cleanup of what a deleted cell defined.
    /// The restored code is the user's, and a restored cell whose code isn't
    /// what last ran is pending, so it shows unrun; an agent reads it before
    /// running it (`run_conflict`). `warnings` says if the cleanup failed.
    pub fn restore_cells(&self, notebook_id: &str, cells: &Value) -> Result<Value, String> {
        let bad = || tools::argument_error("invalid_argument::cells must be a list of {cell_id, expected, code}");
        let mut restores: Vec<(String, String, Option<String>)> = Vec::new();
        for entry in cells.as_array().filter(|cells| !cells.is_empty()).ok_or_else(bad)? {
            let (Some(cell), Some(expected)) = (entry["cell_id"].as_str(), entry["expected"].as_str()) else { return Err(bad()) };
            // Only an explicit null deletes: a cell left without `code` can't be brought back.
            let code = match entry.get("code") {
                Some(Value::Null) => None,
                Some(Value::String(code)) => Some(code.clone()),
                _ => return Err(bad()),
            };
            if restores.iter().any(|(seen, ..)| seen == cell) {
                return Err(tools::argument_error(&format!("invalid_argument::cell {cell} is listed twice")));
            }
            restores.push((cell.to_owned(), expected.to_owned(), code));
        }
        let nb = self.snapshot(notebook_id)?;
        // Ember keeps code as it stores it (`normalise_code`), and compares `expected` that way too.
        let backend = self.backend_of(&nb.id);
        let stored = |code: &str| match backend {
            Backend::Pluto => code.to_owned(),
            Backend::Ember => {
                let code = code.replace("\r\n", "\n");
                let mut lines: Vec<&str> = code.split('\n').collect();
                while lines.last().is_some_and(|line| line.trim().is_empty()) {
                    lines.pop();
                }
                lines.join("\n")
            }
        };
        let restores: Vec<(String, String, Option<String>)> = restores.into_iter().map(|(cell, expected, code)| (cell, stored(&expected), code.map(|code| stored(&code)))).collect();
        let changed: Vec<String> = restores.iter().filter(|(cell, expected, _)| nb.cells.get(cell).is_none_or(|c| c.code != *expected)).map(|(cell, ..)| cell.clone()).collect();
        if !changed.is_empty() {
            return Ok(json!({ "restored": [], "changed": changed }));
        }
        // Edits first: the engine checks every `expected` before it applies any op. A cell to delete was
        // checked against the snapshot just above, not by the engine.
        let mut ops: Vec<Value> = restores.iter().filter_map(|(cell, expected, code)| Some(json!({ "op": "set_code", "cell_id": cell, "code": code.as_ref()?, "expected": expected }))).collect();
        ops.extend(restores.iter().filter(|(.., code)| code.is_none()).map(|(cell, ..)| json!({ "op": "delete", "cell_id": cell })));
        // A cell a run is taking (the turn's `run_after` that wasn't waited for) has its agent code run.
        let pending: Vec<String> = self.with_state(&nb.id, |state| {
            let pending = state.pending_run(&nb);
            let in_run = |cell: &String| nb.cells.get(cell).is_some_and(|c| c.running || c.queued) || state.started.iter().any(|run| run.cells.contains(cell));
            pending.into_iter().filter(|cell| !in_run(cell)).collect()
        });
        if let Err(error) = self.call("apply", json!({ "notebook_id": nb.id, "ops": ops })) {
            // A cell changed between the snapshot and the edit.
            if error.contains("stale_read::") {
                let now = self.snapshot(&nb.id)?;
                let changed: Vec<&String> = restores.iter().filter(|(cell, expected, _)| now.cells.get(cell).is_none_or(|c| c.code != *expected)).map(|(cell, ..)| cell).collect();
                return Ok(json!({ "restored": [], "changed": changed }));
            }
            return Err(error);
        }
        let now = (self.clock)();
        {
            let mut state = self.state.lock().unwrap();
            state.seq += 1;
            let seq = state.seq;
            let notebook = state.notebooks.entry(nb.id.clone()).or_default();
            for (cell, expected, code) in &restores {
                notebook.tool_edits.remove(cell);
                match code {
                    None => {
                        notebook.pending.remove(cell);
                        notebook.befores.remove(cell);
                        notebook.reads.retain(|(_, read), _| read != cell);
                        notebook.changes.remove(cell);
                    }
                    Some(code) => {
                        notebook.authors.insert(cell.clone(), (hash(code), "user"));
                        // The user's change: an agent reads the cell before a run of it or what needs it.
                        notebook.changes.insert(cell.clone(), Change { owner: tools::USER.to_owned(), seq });
                        // Unrun agent code put back to what last ran is in step again; anything else is
                        // unrun, its before-text the code that last ran (the turn's, if the turn ran it).
                        if !pending.contains(cell) {
                            notebook.befores.insert(cell.clone(), expected.clone());
                            notebook.pending.insert(cell.clone(), now);
                        } else if notebook.befores.get(cell) == Some(code) {
                            notebook.befores.remove(cell);
                            notebook.pending.remove(cell);
                        } else {
                            notebook.pending.insert(cell.clone(), now);
                        }
                    }
                }
            }
        }
        self.publish();
        let mut result = json!({ "restored": restores.iter().map(|(cell, ..)| cell).collect::<Vec<_>>(), "changed": [] });
        if restores.iter().any(|(.., code)| code.is_none()) {
            // Pluto's reactive cleanup of what the deleted cells defined, as delete_cell does. The restore
            // is made either way.
            if let Err(error) = self.call("run", json!({ "notebook_id": nb.id, "cells": [], "wait": false, "timeout": tools::TIMEOUT_SECONDS })) {
                result["warnings"] = json!([format!("cleanup_failed::{error}")]);
            }
        }
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
pub(crate) fn uuid_value(value: &Value) -> Option<String> {
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

/// A short cell id as Ember keeps it from a hand-written file (`# %% id=B`):
/// 1 to 64 of `[A-Za-z0-9._-]` (Ember #82). Pluto's cell ids are always UUIDs.
pub(crate) fn short_cell_id(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let valid = (1..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    valid.then(|| text.to_owned())
}

/// The cell `value` names in `nb`: one of its cells by that exact id (an R
/// notebook's short id), else a UUID as Julia's `UUID(value)` takes it.
fn cell_id_in(nb: &Snapshot, value: &Value) -> Option<String> {
    short_cell_id(value).filter(|id| nb.cells.contains_key(id)).or_else(|| uuid_value(value))
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
