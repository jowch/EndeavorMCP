pub(crate) mod engines;

mod r;
mod tools;
pub(crate) use tools::ASK_STEP;

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
const OPENING_KEPT: Duration = Duration::from_secs(600);

pub const IDLE_HOURS: f64 = 48.0;

pub fn idle_check() -> Duration {
    std::env::var("ENDEAVOR_IDLE_CHECK_SECS").ok().and_then(|s| s.parse().ok()).map_or(IDLE_CHECK, Duration::from_secs_f64)
}

pub trait Upstream: Send + Sync {
    fn adapter(&self, raw: &[u8]) -> io::Result<String>;
    fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>>;
    fn why_ended(&self) -> Option<String> {
        None
    }
}

pub type Starter = Box<dyn Fn(Backend, Instant, Duration) -> Result<Arc<dyn Upstream>, String> + Send + Sync>;

pub struct Julia {
    pub port: OnceLock<u16>,
    pub(crate) token: String,
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
        write!(stream, "GET /notifications HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\n\r\n", self.token)?;
        let mut reader = BufReader::new(stream);
        let head = Head::read(&mut reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if head.status() != 200 {
            return Err(io::Error::other(format!("Julia's /notifications answered {}", head.status())));
        }
        Ok(Box::new(reader))
    }
}

struct Change {
    owner: String,
    seq: u64,
    at: f64,
}

#[derive(Default)]
struct NotebookState {
    path: String,
    authors: HashMap<String, (u64, &'static str)>,
    befores: HashMap<String, String>,
    edited_at: HashMap<String, u64>,
    last_active: Option<f64>,
    kept_alive: bool,
    pending: HashMap<String, f64>,
    tool_edits: HashMap<String, f64>,
    user_runs: HashMap<String, (f64, f64)>,
    reads: HashMap<(String, String), (String, u64, u64)>,
    changes: HashMap<String, Change>,
    started: Vec<Started>,
    ended: VecDeque<String>,
    touched: u64,
}

struct Started {
    owner: String,
    run: Option<String>,
    cells: HashSet<String>,
    follows: HashMap<String, f64>,
}

const ENDED_KEPT: usize = 64;

impl NotebookState {
    fn touched_after(&self, since: u64) -> bool {
        self.touched > since || self.reads.values().map(|(_, seq, _)| *seq).chain(self.changes.values().map(|c| c.seq)).any(|seq| seq > since)
    }

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

    fn before(&mut self, cell: &str, code: &str, unrun: bool) -> Option<String> {
        if !unrun {
            self.befores.remove(cell);
            return None;
        }
        self.befores.get(cell).filter(|before| *before != code).cloned()
    }

    fn agent_edited(&mut self, cell: &str, before: &str, code: &str, seq: Option<u64>) {
        self.authors.insert(cell.to_owned(), (hash(code), "agent"));
        if let Some(seq) = seq {
            self.edited_at.insert(cell.to_owned(), seq);
        }
        self.befores.entry(cell.to_owned()).or_insert_with(|| before.to_owned());
    }

    fn pending_run(&mut self, nb: &Snapshot) -> Vec<String> {
        self.pending.retain(|id, edited| !nb.cells.get(id).is_some_and(|c| c.ran_since(*edited)));
        let mut ids: Vec<String> = nb.order.iter().filter(|id| self.pending.contains_key(*id)).cloned().collect();
        let mut gone: Vec<String> = self.pending.keys().filter(|id| !nb.cells.contains_key(*id)).cloned().collect();
        gone.sort();
        ids.extend(gone);
        ids
    }

    fn prune(&mut self, nb: &Snapshot) {
        self.pending.retain(|id, _| nb.cells.contains_key(id));
    }
}

struct State {
    notebooks: HashMap<String, NotebookState>,
    seq: u64,
    idle_limit_hours: f64,
    idle_stopped: Vec<(String, Value)>,
    bindings: HashMap<String, String>,
    several: HashMap<String, HashSet<String>>,
    closed_own: HashSet<String>,
    seen: HashMap<String, Seen>,
    calls: HashMap<String, u64>,
}

struct Seen {
    last_call: Option<f64>,
    since: f64,
    app: bool,
    front: Option<crate::mcp::Front>,
}

const SESSION_KEPT: f64 = 7.0 * 24.0 * 3600.0;

const SHARED_WITHOUT_FRONT: f64 = 30.0 * 60.0;

impl State {
    fn record(&mut self, owner: &str, now: f64) -> &mut Seen {
        if !self.seen.contains_key(owner) {
            self.forget_old(now);
        }
        self.seen.entry(owner.to_owned()).or_insert(Seen { last_call: None, since: now, app: false, front: None })
    }

    fn record_bound(&mut self, owner: &str, now: f64) {
        self.record(owner, now).since = now;
    }

    fn forget_old(&mut self, now: f64) {
        let bindings = &mut self.bindings;
        let several = &mut self.several;
        let closed_own = &mut self.closed_own;
        self.seen.retain(|session, seen| {
            let kept = now - seen.last_call.unwrap_or(f64::MIN).max(seen.since) < SESSION_KEPT;
            if !kept {
                bindings.remove(session);
                several.remove(session);
                closed_own.remove(session);
            }
            kept
        });
    }
}

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
    notify: OnceLock<Sender<Value>>,
    pub starter: OnceLock<Starter>,
    starting: Mutex<()>,
    notices: Mutex<Vec<String>>,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    state: Mutex<State>,
    publishing: Mutex<()>,
    events: Mutex<Events>,
    pub build: OnceLock<String>,
    pub exits_when_idle: std::sync::atomic::AtomicBool,
    pub stopping: std::sync::atomic::AtomicBool,
    pub asks: Asks,
    kinds: Mutex<HashMap<String, Backend>>,
    auto: Mutex<HashSet<String>>,
    openings: Mutex<HashMap<String, Arc<Opening>>>,
    left: Mutex<Vec<(String, Arc<Opening>)>>,
    runs: Mutex<Vec<(String, Value, String, bool)>>,
}

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

fn adapter_call(engines: &Engines, message: &Value) -> Result<Value, String> {
    let reply = engines.adapter(message.to_string().as_bytes()).map_err(|e| e.to_string())?;
    let mut reply: Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
    match reply.get("error") {
        Some(error) => Err(julia_string(error)),
        None => Ok(reply["result"].take()),
    }
}

struct Snapshot {
    seq: Option<u64>,
    id: String,
    path: String,
    order: Vec<String>,
    execution_allowed: bool,
    safe_preview: bool,
    process_status: Value,
    exited: Option<Vec<String>>,
    packages: Option<Value>,
    lazy: bool,
    package_date: Option<String>,
    cells: HashMap<String, Cell>,
}

struct Cell {
    code: String,
    folded: bool,
    running: bool,
    queued: bool,
    errored: bool,
    last_run: f64,
    runtime: f64,
    output: String,
    error: Option<Value>,
    hidden: bool,
    markdown: bool,
    stale: bool,
    not_run: bool,
    off: bool,
    notes: Vec<String>,
    inputs: Vec<String>,
    prompt: Option<Value>,
}

impl Cell {
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
                off: flag(&c["off"]),
                notes: c["notes"].as_array().map(|a| a.iter().filter_map(text).collect()).unwrap_or_default(),
                inputs: c["inputs"].as_array().map(|a| a.iter().filter_map(text).collect()).unwrap_or_default(),
                prompt: c.get("prompt").filter(|p| p.is_object()).cloned(),
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

    fn is_running(&self, cell: &Cell) -> bool {
        cell.running || (cell.queued && (self.packages.is_some() || self.cells.values().any(|c| c.running)))
    }

    fn prompted(&self) -> Option<&String> {
        self.order.iter().find(|id| self.cells.get(*id).is_some_and(|c| c.running && c.prompt.is_some()))
    }

    fn installing(&self) -> bool {
        self.packages.is_some() && self.cells.values().any(|c| c.queued)
    }

    fn asking(&self) -> bool {
        self.packages.as_ref().is_some_and(|p| p["step"] == "asking")
    }

    fn held(&self) -> bool {
        self.asking() && !self.cells.values().any(|c| c.running)
    }

    fn summary(&self, pending_run: &[String], this_session: bool) -> Value {
        let held = self.held();
        let running: Vec<&String> = self.order.iter().filter(|id| self.cells.get(*id).is_some_and(|c| self.is_running(c) && !held)).collect();
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

#[derive(Default)]
struct Graph {
    cells: Vec<Node>,
    order: Vec<String>,
}

struct Node {
    id: String,
    definitions: Vec<String>,
    functions: Vec<String>,
    references: Vec<String>,
    upstream: Vec<String>,
    downstream: Vec<String>,
    packages: Vec<String>,
}

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

    fn name(&self, id: &str) -> Option<String> {
        let node = self.node(id)?;
        let mut defs: Vec<&str> = node.definitions.iter().chain(&node.functions).map(String::as_str).collect();
        defs.sort_unstable();
        defs.dedup();
        (!defs.is_empty()).then(|| defs[..defs.len().min(3)].join(", "))
    }

    fn upstream_of(&self, from: &[String]) -> HashSet<String> {
        self.closure(from, |n| &n.upstream)
    }

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
    pub fn new(pluto: Option<Arc<dyn Upstream>>, clock: Box<dyn Fn() -> f64 + Send + Sync>) -> Notebooks {
        Notebooks {
            asks: Asks::new(clock()),
            engines: Arc::new(Engines::new(pluto)),
            notify: OnceLock::new(),
            starter: OnceLock::new(),
            starting: Mutex::default(),
            notices: Mutex::default(),
            clock,
            state: Mutex::new(State { notebooks: HashMap::new(), seq: 0, idle_limit_hours: IDLE_HOURS, idle_stopped: Vec::new(), bindings: HashMap::new(), several: HashMap::new(), closed_own: HashSet::new(), seen: HashMap::new(), calls: HashMap::new() }),
            publishing: Mutex::default(),
            events: Mutex::default(),
            build: OnceLock::new(),
            exits_when_idle: Default::default(),
            stopping: Default::default(),
            kinds: Mutex::default(),
            auto: Mutex::default(),
            openings: Mutex::default(),
            left: Mutex::default(),
            runs: Mutex::default(),
        }
    }

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

    pub fn add_engine(&self, backend: Backend, upstream: Arc<dyn Upstream>) {
        if self.engines.add(backend, upstream.clone())
            && let Some(tx) = self.notify.get()
        {
            self.follow(backend, upstream, tx.clone());
        }
    }

    pub fn warn_next(&self, warning: String) {
        self.notices.lock().unwrap().push(warning);
    }

    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap())
    }

    pub fn backend_of(&self, id: &str) -> Backend {
        self.engines.backend_of(id)
    }

    pub(crate) fn start_engine(&self, backend: Backend, wait: Duration) -> Result<(), String> {
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

    fn follow(&self, backend: Backend, upstream: Arc<dyn Upstream>, tx: Sender<Value>) {
        let engines = self.engines.clone();
        std::thread::spawn(move || {
            while engines.is_current(backend, &upstream) {
                let stream = match upstream.notifications() {
                    Ok(stream) => stream,
                    Err(_) if backend != Backend::Pluto && let Err(e) = upstream.adapter(br#"{"method":"status","params":{}}"#) => {
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
            "run_finished" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    let finished: Vec<&str> = params["cells"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
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
            "process_exited" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    notebook.started.clear();
                }
                true
            }
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

    pub fn open_in(&self, id: &str) -> Option<wire::backend::Backend> {
        self.snapshots().ok()?.iter().find(|nb| nb.id == id).map(|nb| engines::of_path(&nb.path))
    }

    pub fn ember_page(&self, id: &str) -> Result<String, String> {
        let reply = self.call("page_bundle", json!({ "notebook_id": id })).map_err(|e| match e.trim_start_matches("ArgumentError: ").split_once("::") {
            Some(("update_ember", said)) => format!("update_ember: {said}"),
            _ => format!("R couldn't make Ember's page: {e}"),
        })?;
        reply["html"].as_str().map(str::to_owned).ok_or_else(|| "R couldn't make Ember's page: no page".to_owned())
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        adapter_call(&self.engines, &json!({ "method": method, "params": params }))
    }

    fn opening(&self, key: &str) -> bool {
        self.openings.lock().unwrap().get(key).is_some_and(|opening| opening.done.lock().unwrap().as_ref().is_none_or(|(_, at)| at.elapsed() < OPENING_KEPT))
    }

    fn open_waited(&self, owner: &str, method: &str, params: Value, key: String, wait: Duration, doing: &str) -> Result<(Value, Value), String> {
        let began = Instant::now();
        let backend = engines::of_params(&params);
        self.start_engine(backend, wait)?;
        let why = engines::slow_start(backend, false);
        self.waited(owner, method, params, key, (began + wait).saturating_duration_since(Instant::now()), &format!("{doing} is taking longer than one call waits: {why}"))
    }

    fn waited(&self, owner: &str, method: &str, params: Value, key: String, wait: Duration, taking: &str) -> Result<(Value, Value), String> {
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
                drop(openings);
                self.left.lock().unwrap().retain(|(o, left)| !(o == owner && Arc::ptr_eq(left, &opening)));
                return result.map(|result| (result, opening.params.clone()));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(done);
                if !owner.is_empty() && (method == "open" || method == "new") {
                    let mut left = self.left.lock().unwrap();
                    if !left.iter().any(|(o, l)| o == owner && Arc::ptr_eq(l, &opening)) {
                        left.push((owner.to_owned(), opening.clone()));
                    }
                }
                return Err(format!("notebook_opening::{taking}. It goes on by itself. Make the same call again to wait for it."));
            }
            done = opening.came.wait_timeout(done, left).unwrap().0;
        }
    }

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

    fn with_state<T>(&self, id: &str, f: impl FnOnce(&mut NotebookState) -> T) -> T {
        let mut state = self.state.lock().unwrap();
        state.seq += 1;
        let seq = state.seq;
        let notebook = state.notebooks.entry(id.to_owned()).or_default();
        notebook.touched = seq;
        f(notebook)
    }

    fn compose(&self) -> Result<String, String> {
        let since = self.state.lock().unwrap().seq;
        let mut snapshots = self.snapshots()?;
        let mut graphs = HashMap::new();
        for nb in &snapshots {
            match self.graph(&nb.id, GraphQuery::default()) {
                Ok(graph) => {
                    graphs.insert(nb.id.clone(), graph);
                }
                Err(e) if e.contains("notebook_not_found::") => {}
                Err(e) => return Err(e),
            }
        }
        snapshots.retain(|nb| graphs.contains_key(&nb.id));
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
        let engine_ended: Vec<Value> = self.engines.closed().iter().map(|c| json!({ "path": c.path, "language": c.language, "why": c.why, "ended_at": c.ended_at })).collect();
        let mut event = json!({ "notebooks": list, "cells": cells, "idle_stopped": idle_stopped, "engine_ended": engine_ended, "asks": self.asks.list() });
        if let Some(build) = self.build.get() {
            event["build"] = build.clone().into();
        }
        Ok(to_json(&event))
    }

    pub fn publish(&self) {
        let _publishing = self.publishing.lock().unwrap();
        let json = match self.compose() {
            Ok(json) => json,
            Err(_) if self.stopping.load(std::sync::atomic::Ordering::SeqCst) => return,
            Err(e) => return eprintln!("┌ Warning: Couldn't summarize notebooks for the app: {e}"),
        };
        let mut events = self.events.lock().unwrap();
        if json != events.last {
            events.last = json.clone();
            events.subscribers.retain(|subscriber| subscriber.send(json.clone()).is_ok());
        }
    }

    pub fn followed(&self) -> bool {
        !self.events.lock().unwrap().subscribers.is_empty()
    }

    fn subscribe(&self) -> Result<(String, Receiver<String>), String> {
        let _publishing = self.publishing.lock().unwrap();
        let first = self.compose()?;
        let (tx, rx) = mpsc::channel();
        self.events.lock().unwrap().subscribers.push(tx);
        Ok((first, rx))
    }

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

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    pub fn note_activity(&self, arguments: &Value) {
        let Some(id) = arguments.get("notebook_id").map(julia_string).and_then(|id| parse_uuid(&id)) else { return };
        let now = (self.clock)();
        if let Some(notebook) = self.state.lock().unwrap().notebooks.get_mut(&id) {
            notebook.last_active = Some(now);
        }
    }

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

    fn notebook_arg(&self, value: &Value) -> Result<String, String> {
        let shown = julia_string(value);
        let id = uuid_value(value).ok_or_else(|| format!("ArgumentError: invalid_notebook_id::Invalid notebook ID: '{shown}'"))?;
        let known = self.state.lock().unwrap().notebooks.contains_key(&id);
        if known || self.snapshot(&id).is_ok() {
            return Ok(id);
        }
        Err(tools::notebook_not_found(&shown, self.open_now().as_deref()))
    }

    fn open_now(&self) -> Option<Vec<(String, String)>> {
        self.snapshots().ok().map(|open| open.into_iter().map(|nb| (nb.id, nb.path)).collect())
    }

    pub fn bind(&self, owner: &str, path: &str) {
        let now = self.now();
        let mut state = self.state.lock().unwrap();
        state.closed_own.remove(owner);
        if path.is_empty() {
            state.bindings.remove(owner);
        } else {
            state.bindings.insert(owner.to_owned(), canonical_path(path).unwrap_or_else(|_| path.to_owned()));
            state.record_bound(owner, now);
        }
    }

    pub fn set_kind(&self, owner: &str, kind: Option<Backend>) {
        let mut kinds = self.kinds.lock().unwrap();
        match kind {
            Some(kind) => kinds.insert(owner.to_owned(), kind),
            None => kinds.remove(owner),
        };
    }

    pub fn set_auto(&self, owner: &str, auto: bool) {
        let mut sessions = self.auto.lock().unwrap();
        if auto { sessions.insert(owner.to_owned()) } else { sessions.remove(owner) };
    }

    pub fn asks_new_packages(&self, owner: &str) -> bool {
        !self.auto.lock().unwrap().contains(owner)
    }

    pub fn kind(&self, owner: &str) -> Option<Backend> {
        self.kinds.lock().unwrap().get(owner).copied()
    }

    pub fn code_print(&self, id: &str) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let nb = self.snapshot(id).ok()?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for cell in &nb.order {
            (cell, nb.cells.get(cell).map(|c| &c.code)).hash(&mut hasher);
        }
        Some(hasher.finish())
    }

    pub fn note_call(&self, owner: &str, app: bool, front: Option<&crate::mcp::Front>) {
        if owner.is_empty() {
            return;
        }
        let now = self.now();
        let mut state = self.state.lock().unwrap();
        let seen = state.record(owner, now);
        seen.last_call = Some(now);
        seen.app = app;
        if front.is_some() {
            seen.front = front.cloned();
        }
    }

    pub fn shared(&self, alive: &dyn Fn(&crate::mcp::Front) -> bool) -> bool {
        let Ok(snapshots) = self.snapshots() else { return false };
        let open: Vec<String> = snapshots.into_iter().map(|nb| canonical_path(&nb.path).unwrap_or(nb.path)).collect();
        let now = self.now();
        let state = self.state.lock().unwrap();
        state.bindings.iter().filter(|(_, path)| open.contains(path)).filter_map(|(session, _)| state.seen.get(session)).any(|seen| {
            !seen.app
                && match &seen.front {
                    Some(front) => alive(front),
                    None => seen.last_call.is_some_and(|last| now - last <= SHARED_WITHOUT_FRONT),
                }
        })
    }

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
        self.settle();
        self.state.lock().unwrap().bindings.get(owner).cloned()
    }

    fn opened_by(&self, owner: &str, path: &str, id: &str) {
        if !owner.is_empty() {
            let path = canonical_path(path).unwrap_or_else(|_| path.to_owned());
            let now = self.now();
            let mut state = self.state.lock().unwrap();
            if state.bindings.entry(owner.to_owned()).or_insert(path.clone()) == &path {
                state.closed_own.remove(owner);
            }
            if let Some(opened) = state.several.get_mut(owner) {
                opened.insert(id.to_owned());
            }
            state.record_bound(owner, now);
        }
    }

    fn settle(&self) {
        let came: Vec<(String, Value)> = {
            let mut left = self.left.lock().unwrap();
            let mut came = Vec::new();
            left.retain(|(owner, opening)| match opening.done.lock().unwrap().as_ref() {
                None => true,
                Some((Ok(opened), _)) => {
                    came.push((owner.clone(), opened.clone()));
                    false
                }
                Some((Err(_), _)) => false,
            });
            came
        };
        for (owner, opened) in came {
            if let (Some(path), Some(id)) = (opened["path"].as_str(), opened["notebook_id"].as_str()) {
                self.opened_by(&owner, path, id);
            }
        }
    }

    fn unbind(&self, owner: &str, path: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.closed_own.contains(owner) && state.bindings.get(owner).is_some_and(|bound| bound == path) {
            state.bindings.remove(owner);
            return true;
        }
        false
    }

    fn seen_open(&self, owner: &str) {
        self.state.lock().unwrap().closed_own.remove(owner);
    }

    fn closed_by(&self, owner: &str, path: &str) {
        let mut state = self.state.lock().unwrap();
        if !owner.is_empty() && !state.several.contains_key(owner) && state.bindings.get(owner).is_some_and(|bound| bound == path) {
            state.closed_own.insert(owner.to_owned());
        }
    }

    pub fn allow_several(&self, owner: &str) {
        if !owner.is_empty() {
            self.state.lock().unwrap().several.entry(owner.to_owned()).or_default();
        }
    }

    fn several(&self, owner: &str) -> bool {
        self.state.lock().unwrap().several.contains_key(owner)
    }

    fn several_refusal(&self, owner: &str, tool: &str, arguments: &Value, folder: &Folder) -> Option<String> {
        if tool == "open_notebook" || tool == "new_notebook" {
            let requested = arguments.get("path").and_then(Value::as_str)?;
            return requested_path(requested, folder).and_then(|path| canonical_path(&path)).err();
        }
        if !WRITE_TOOLS.contains(&tool) || tool == "run_shell" {
            return None;
        }
        let id = parse_uuid(&julia_string(arguments.get("notebook_id").unwrap_or(&Value::String(String::new()))))?;
        if self.state.lock().unwrap().several.get(owner).is_some_and(|opened| opened.contains(&id)) {
            return None;
        }
        let known = self.state.lock().unwrap().notebooks.get(&id).map(|nb| nb.path.clone()).filter(|p| !p.is_empty());
        let path = known.or_else(|| self.snapshot(&id).ok().map(|nb| nb.path))?;
        Some(format!(
            "ArgumentError: one_notebook::{path} wasn't opened or created here, so you can't change or run it. \
             If the user wants you to work on it, open it with `open_notebook` and its path first."
        ))
    }

    pub fn refusal(&self, owner: &str, tool: &str, arguments: &Value, folder: &Folder) -> Option<String> {
        if owner.is_empty() {
            return None;
        }
        self.settle();
        if self.several(owner) {
            return self.several_refusal(owner, tool, arguments, folder);
        }
        let bound = self.bound(owner)?;
        let refuse = |what: &str| {
            let open = self.snapshots().ok().map(|open| open.iter().any(|nb| canonical_path(&nb.path).is_ok_and(|p| p == bound)));
            if open == Some(false) && self.unbind(owner, &bound) {
                return None;
            }
            if open == Some(true) {
                self.seen_open(owner);
            }
            let closed = if open == Some(false) && self.state.lock().unwrap().closed_own.contains(owner) {
                " You closed it, and closing it doesn't free the session for another. If the user didn't ask you to close it, open it again now with open_notebook and its path, so their notebook isn't left closed, and go on there."
            } else {
                ""
            };
            Some(format!(
                "ArgumentError: one_notebook::This session works on one notebook, {bound}, so it can't {what}.{closed} \
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
            Ok(canonical) if canonical == bound => {
                if self.state.lock().unwrap().closed_own.contains(owner) && self.snapshot(&id).is_ok() {
                    self.seen_open(owner);
                }
                None
            }
            Err(error) => Some(error),
            Ok(_) => refuse(&format!("change or run {path}")),
        }
    }

    pub fn set_idle_limit(&self, hours: f64) {
        self.state.lock().unwrap().idle_limit_hours = hours;
    }

    pub fn idle_limit_hours(&self) -> f64 {
        self.state.lock().unwrap().idle_limit_hours
    }

    pub fn open_and_allowed(&self, path: &str) -> bool {
        let Ok(target) = canonical_path(path) else { return false };
        self.snapshots().is_ok_and(|snapshots| snapshots.iter().any(|nb| nb.execution_allowed && !nb.safe_preview && canonical_path(&nb.path).is_ok_and(|p| p == target)))
    }

    pub fn open_count(&self) -> Option<usize> {
        self.snapshots().ok().map(|snapshots| snapshots.len())
    }

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
            eprintln!("[ Info: Stopped {} after {} idle", nb.path, idle_span(hours));
        }
        idle.into_iter().map(|nb| nb.path).collect()
    }

    fn shutdown(&self, id: &str) -> Result<bool, String> {
        let result = self.call("shutdown", json!({ "notebook_id": id }))?;
        self.state.lock().unwrap().notebooks.remove(id);
        Ok(result["safe_preview"].as_bool().unwrap_or(false))
    }

    pub fn stop_notebook(&self, path: &str) -> Result<Value, String> {
        let target = canonical_path(path)?;
        let snapshots = self.snapshots()?;
        let Some(nb) = snapshots.iter().find(|nb| canonical_path(&nb.path).is_ok_and(|p| p == target)) else {
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

    pub fn running(&self, owner: &str, request: &Value, notebook_id: &str, runs_code: bool) -> Running<'_> {
        self.runs.lock().unwrap().push((owner.to_owned(), request.clone(), notebook_id.to_owned(), runs_code));
        Running { notebooks: self, owner: owner.to_owned(), request: request.clone() }
    }

    pub fn cancelled(&self, owner: &str, request: &Value) -> Option<(String, bool)> {
        let runs = self.runs.lock().unwrap();
        runs.iter().find(|(o, r, _, _)| o == owner && r == request).map(|(_, _, notebook, runs_code)| (notebook.clone(), *runs_code))
    }

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

    fn own_run_will_run(&self, owner: &str, nb: &Snapshot, cell: &str) -> bool {
        let Some(c) = nb.cells.get(cell) else { return false };
        self.with_state(&nb.id, |state| state.started.iter().any(|started| started.owner == owner && started.follows.get(cell).is_some_and(|last| c.last_run == *last)))
    }

    pub fn session_runs(&self, owner: &str) -> Vec<String> {
        if owner.is_empty() {
            return Vec::new();
        }
        let state = self.state.lock().unwrap();
        let mut ids: Vec<String> = state.notebooks.iter().filter(|(_, nb)| nb.started.iter().any(|started| started.owner == owner)).map(|(id, _)| id.clone()).collect();
        ids.sort();
        ids
    }

    pub fn interrupt(&self, notebook_id: &str, owner: &str) {
        match self.call("interrupt", json!({ "notebook_id": notebook_id })) {
            Ok(reply) if reply["interrupted"] == false => eprintln!("[ Info: Session {owner} stopped: nothing was running in notebook {notebook_id}"),
            Ok(_) => eprintln!("[ Info: Interrupted notebook {notebook_id}: session {owner} stopped"),
            Err(error) => eprintln!("┌ Warning: Couldn't interrupt notebook {notebook_id} for session {owner}'s Stop: {error}"),
        }
    }

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

    pub fn restart(&self, notebook_id: &str, ask_new: bool) -> Result<Value, String> {
        self.with_state(notebook_id, |state| state.started.clear());
        let result = self.call("restart", json!({ "notebook_id": notebook_id, "timeout": tools::TIMEOUT_SECONDS, "ask_new": ask_new }))?;
        self.publish();
        Ok(result)
    }

    pub fn restore_cells(&self, notebook_id: &str, cells: &Value) -> Result<Value, String> {
        let bad = || tools::argument_error("invalid_argument::cells must be a list of {cell_id, expected, code}");
        let mut restores: Vec<(String, String, Option<String>)> = Vec::new();
        for entry in cells.as_array().filter(|cells| !cells.is_empty()).ok_or_else(bad)? {
            let (Some(cell), Some(expected)) = (entry["cell_id"].as_str(), entry["expected"].as_str()) else { return Err(bad()) };
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
        let backend = self.backend_of(&nb.id);
        let stored = |code: &str| match backend {
            Backend::Pluto | Backend::Marimo => code.to_owned(),
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
        let mut ops: Vec<Value> = restores.iter().filter_map(|(cell, expected, code)| Some(json!({ "op": "set_code", "cell_id": cell, "code": code.as_ref()?, "expected": expected }))).collect();
        ops.extend(restores.iter().filter(|(.., code)| code.is_none()).map(|(cell, ..)| json!({ "op": "delete", "cell_id": cell })));
        let pending: Vec<String> = self.with_state(&nb.id, |state| {
            let pending = state.pending_run(&nb);
            let in_run = |cell: &String| nb.cells.get(cell).is_some_and(|c| c.running || c.queued) || state.started.iter().any(|run| run.cells.contains(cell));
            pending.into_iter().filter(|cell| !in_run(cell)).collect()
        });
        if let Err(error) = self.call("apply", json!({ "notebook_id": nb.id, "ops": ops })) {
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
                        notebook.changes.insert(cell.clone(), Change { owner: tools::USER.to_owned(), seq, at: self.now() });
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
        if restores.iter().any(|(.., code)| code.is_none()) && let Err(error) = self.call("run", json!({ "notebook_id": nb.id, "cells": [], "wait": false, "timeout": tools::TIMEOUT_SECONDS })) {
            result["warnings"] = json!([format!("cleanup_failed::{error}")]);
        }
        Ok(result)
    }

    pub fn move_notebook(&self, notebook_id: &str, path: &str) -> Result<Value, String> {
        let nb = self.snapshot(notebook_id)?;
        let target = absolute_path(path)?;
        let (fits, extension) = match self.backend_of(&nb.id) {
            Backend::Pluto => (target.ends_with(".jl"), ".jl"),
            Backend::Ember => (engines::of_path(&target) == Backend::Ember, ".R"),
            Backend::Marimo => (target.ends_with(".py"), ".py"),
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

pub fn file_info(path: &str) -> Result<Value, String> {
    match std::fs::metadata(absolute_path(path)?) {
        Ok(meta) if meta.is_file() => Ok(json!({ "exists": true, "modified": crate::host_tools::mtime(&meta) })),
        _ => Ok(json!({ "exists": false })),
    }
}

fn hash(code: &str) -> u64 {
    code.trim_end().bytes().filter(|&b| b != b'\r').fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn parse_uuid(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let hyphen = |i: usize| matches!(i, 8 | 13 | 18 | 23);
    let valid = bytes.len() == 36 && bytes.iter().enumerate().all(|(i, b)| if hyphen(i) { *b == b'-' } else { b.is_ascii_hexdigit() });
    valid.then(|| text.to_ascii_lowercase())
}

pub(crate) fn seen_packages(asked: Option<&Value>, seen: &Value) -> Value {
    let names = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
    let key = |name: &str| name.to_lowercase().replace(['_', '.'], "-");
    let seen = names(seen);
    match asked.filter(|a| a.is_array()) {
        Some(asked) => {
            let asked: Vec<String> = names(asked).iter().map(|p| key(p)).collect();
            json!(seen.into_iter().filter(|p| asked.contains(&key(p))).collect::<Vec<_>>())
        }
        None => json!(seen),
    }
}

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

pub(crate) fn short_cell_id(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let valid = (1..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    valid.then(|| text.to_owned())
}

fn cell_id_in(nb: &Snapshot, value: &Value) -> Option<String> {
    short_cell_id(value).filter(|id| nb.cells.contains_key(id)).or_else(|| uuid_value(value))
}

fn uuid_of(value: u128) -> String {
    let hex = format!("{value:032x}");
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

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

#[derive(Clone, Debug, PartialEq)]
pub enum Folder {
    In(String),
    Process,
    Unknown,
}

impl Folder {
    pub fn dir(&self) -> Option<&str> {
        match self {
            Folder::In(dir) => Some(dir),
            _ => None,
        }
    }
}

fn folder_unknown() -> String {
    tools::argument_error("invalid_path::Give an absolute path: this server was not told the project folder.")
}

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

fn is_absolute(path: &str) -> bool {
    if cfg!(windows) {
        let drive = path.find(':').filter(|&colon| colon > 0 && path[..colon].bytes().all(|b| b.is_ascii_alphabetic())).map_or(0, |colon| colon + 1);
        return path[drive..].starts_with(['/', '\\']);
    }
    path.starts_with('/')
}

fn is_fully_absolute(path: &str) -> bool {
    if cfg!(windows) {
        let drive = path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':';
        return (drive && path[2..].starts_with(['/', '\\'])) || path.starts_with("\\\\") || path.starts_with("//");
    }
    path.starts_with('/')
}

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

pub(crate) fn idle_span(hours: f64) -> String {
    if hours < 1.0 {
        let minutes = ((hours * 60.0).round() as i64).max(1);
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else {
        let hours = hours.round_ties_even() as i64;
        format!("{hours} hour{}", if hours == 1 { "" } else { "s" })
    }
}

#[cfg(test)]
mod tests;
