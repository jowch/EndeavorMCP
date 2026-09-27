//! What the core keeps about each open notebook (docs/runtime-core.md): who
//! last changed each cell and the code the agent's edits replaced, idle stop,
//! each agent session's one notebook, and the `/events` stream the app
//! follows. The engine's adapter (Julia's, for Pluto) answers `snapshot`,
//! `graph` and `shutdown` on its `POST /adapter`, and says when a notebook
//! changed on its `GET /notifications` stream; the core reads a fresh snapshot
//! each time it tells the app anything. A notebook's state goes when it shuts
//! down, except its entry in `idle_stopped`.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::host_tools::{home, normpath};
use crate::http::{self, Head};
use crate::mcp::{Caller, WRITE_TOOLS, julia_string, to_json};

const IDLE_CHECK: Duration = Duration::from_secs(300);

/// Tools whose writes to a cell's code the core attributes to the agent.
const EDIT_TOOLS: [&str; 3] = ["edit_cell", "edit_cells", "add_cell"];

/// Where the core sends what it doesn't answer: the engine's bridge.
pub trait Upstream: Send + Sync {
    /// The reply to a request on `path`.
    fn ask(&self, path: &str, raw: &[u8], caller: &Caller) -> io::Result<String>;
    /// The engine's notification stream, from its start.
    fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>>;
}

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
    fn ask(&self, path: &str, raw: &[u8], caller: &Caller) -> io::Result<String> {
        let port = *self.port.get().ok_or(io::ErrorKind::NotConnected)?;
        let authorization = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", authorization.as_str()),
            ("Content-Type", "application/json"),
            ("X-Endeavor-Session", caller.owner.as_str()),
            ("X-Endeavor-Host", caller.host.as_str()),
        ];
        let headers: Vec<_> = headers.into_iter().filter(|(_, value)| !value.is_empty()).collect();
        let (status, body) = http::post(port, path, &headers, raw)?;
        if status != 200 {
            return Err(io::Error::other(format!("Julia's {path} answered {status}")));
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

#[derive(Default)]
struct NotebookState {
    path: String,
    /// Each cell's code (hashed) as last seen, and who made it so: "agent",
    /// "user", or "" if it hasn't changed since the core first saw it.
    authors: HashMap<String, (u64, &'static str)>,
    /// Each unrun cell's code before the agent's first edit since it last ran.
    befores: HashMap<String, String>,
    last_active: Option<f64>,
    kept_alive: bool,
}

impl NotebookState {
    /// Code that changed without the agent's tools writing it was changed by the user.
    fn author(&mut self, cell: &str, code: &str) -> Option<&'static str> {
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
}

struct State {
    notebooks: HashMap<String, NotebookState>,
    idle_limit_hours: f64,
    /// Notebooks the idle check stopped, by canonical path, until they open
    /// again: the app reads these to say why a notebook stopped.
    idle_stopped: Vec<(String, Value)>,
    /// One notebook per agent session: owner => the canonical path it works on.
    /// The app binds a session started from an existing notebook; otherwise
    /// the first notebook the session opens or creates binds it.
    bindings: HashMap<String, String>,
}

#[derive(Default)]
struct Events {
    subscribers: Vec<Sender<String>>,
    last: String,
}

pub struct Notebooks {
    upstream: Arc<dyn Upstream>,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    state: Mutex<State>,
    /// Held while reading the engine's state and telling the app, so events go out in order.
    publishing: Mutex<()>,
    events: Mutex<Events>,
    /// Held across an edit tool call, so the code it replaced is what it replaced.
    editing: Mutex<()>,
}

/// One notebook as the engine's `snapshot` reports it.
struct Snapshot {
    id: String,
    path: String,
    order: Vec<String>,
    execution_allowed: bool,
    safe_preview: bool,
    /// Cells edited through the tools and not run since (the engine keeps
    /// staging until it moves to the core).
    pending_run: Vec<Value>,
    cells: HashMap<String, Cell>,
}

struct Cell {
    code: String,
    running: bool,
    queued: bool,
    errored: bool,
}

impl Snapshot {
    fn parse(value: &Value) -> Option<Snapshot> {
        let text = |v: &Value| v.as_str().map(str::to_owned);
        let flag = |v: &Value| v.as_bool().unwrap_or(false);
        let cells = value["cells"].as_array()?.iter().map(|c| {
            let cell = Cell { code: text(&c["code"])?, running: flag(&c["running"]), queued: flag(&c["queued"]), errored: flag(&c["errored"]) };
            Some((text(&c["cell_id"])?, cell))
        });
        Some(Snapshot {
            id: text(&value["notebook_id"])?,
            path: text(&value["path"])?,
            order: value["cell_order"].as_array()?.iter().filter_map(text).collect(),
            execution_allowed: flag(&value["execution_allowed"]),
            safe_preview: flag(&value["safe_preview"]),
            pending_run: value["pending_run"].as_array().cloned().unwrap_or_default(),
            cells: cells.collect::<Option<_>>()?,
        })
    }

    /// Pluto marks every cell `queued` when it loads a notebook, ahead of the
    /// planned run; in safe preview that run never happens, so the flag
    /// lingers. A queued cell only counts while the notebook is running something.
    fn is_running(&self, cell: &Cell) -> bool {
        cell.running || (cell.queued && self.cells.values().any(|c| c.running))
    }
}

impl Notebooks {
    pub fn new(upstream: Arc<dyn Upstream>, clock: Box<dyn Fn() -> f64 + Send + Sync>) -> Notebooks {
        Notebooks {
            upstream,
            clock,
            state: Mutex::new(State { notebooks: HashMap::new(), idle_limit_hours: 48.0, idle_stopped: Vec::new(), bindings: HashMap::new() }),
            publishing: Mutex::default(),
            events: Mutex::default(),
            editing: Mutex::default(),
        }
    }

    /// Follow the engine's notifications and check for idle notebooks, once it answers.
    pub fn start(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel();
        let notebooks = self.clone();
        std::thread::spawn(move || {
            loop {
                if let Ok(stream) = notebooks.upstream.notifications() {
                    // What changed while no stream was open.
                    let _ = tx.send(json!({ "method": "resync" }));
                    for line in stream.lines().map_while(Result::ok) {
                        if let Some(message) = line.strip_prefix("data: ").and_then(|m| serde_json::from_str(m).ok()) {
                            let _ = tx.send(message);
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let notebooks = self.clone();
        std::thread::spawn(move || notebooks.handle_notifications(rx));
        let notebooks = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(IDLE_CHECK);
                notebooks.stop_idle();
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
            // Seen as each state comes, so an edit made and undone between two
            // events still counts as the user's.
            "cell_state" => {
                if let Some(notebook) = state.notebooks.get_mut(&id) {
                    for cell in params["cells"].as_array().into_iter().flatten() {
                        if let (Some(cell), Some(code)) = (cell["cell_id"].as_str(), cell["code"].as_str()) {
                            notebook.author(cell, code);
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
        let message = json!({ "method": method, "params": params });
        let reply = self.upstream.ask("/adapter", message.to_string().as_bytes(), &Caller::default()).map_err(|e| e.to_string())?;
        let mut reply: Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
        match reply.get("error") {
            Some(error) => Err(julia_string(error)),
            None => Ok(reply["result"].take()),
        }
    }

    /// Every open notebook, in the engine's order.
    fn snapshots(&self) -> Result<Vec<Snapshot>, String> {
        let all = self.call("snapshot", json!({}))?;
        all["notebooks"].as_array().ok_or("snapshot has no notebooks")?.iter().map(|nb| Snapshot::parse(nb).ok_or_else(|| format!("bad snapshot {nb}"))).collect()
    }

    fn snapshot(&self, id: &str) -> Result<Snapshot, String> {
        Snapshot::parse(&self.call("snapshot", json!({ "notebook_id": id }))?).ok_or_else(|| "bad snapshot".into())
    }

    /// Each cell's name: what it defines, as of the engine's last analysis.
    fn names(&self, id: &str) -> Result<HashMap<String, String>, String> {
        let graph = self.call("graph", json!({ "notebook_id": id }))?;
        let mut names = HashMap::new();
        for cell in graph["cells"].as_array().into_iter().flatten() {
            let mut defs: Vec<&str> = ["definitions", "functions"].iter().flat_map(|k| cell[*k].as_array().into_iter().flatten()).filter_map(Value::as_str).collect();
            defs.sort_unstable();
            defs.dedup();
            if let (Some(cell), false) = (cell["cell_id"].as_str(), defs.is_empty()) {
                names.insert(cell.to_owned(), defs[..defs.len().min(3)].join(", "));
            }
        }
        Ok(names)
    }

    /// Every notebook's state as the app hears it, with what the core keeps
    /// brought up to date:
    ///   {"notebooks": [the list_notebooks summary],
    ///    "cells": {notebook_id: [{cell_id, running, errored, unrun, author, before, version, name}, ...]},
    ///    "idle_stopped": [{path, hours, safe_preview}, ...]}
    /// with cells in notebook order. `unrun`: edited by the agent and not run
    /// since. `author`: who last changed the cell's code ("agent", "user", or
    /// null if unchanged since the core first saw it). `before`: an unrun
    /// cell's code before the agent's first edit since it last ran ("" for a
    /// cell the agent added), for the in-editor diff; null otherwise.
    /// `version`: a hash of the code, so the app sees each edit. `name`: what
    /// the cell defines, as of its last run (null if nothing).
    fn compose(&self) -> Result<String, String> {
        let snapshots = self.snapshots()?;
        let names = snapshots.iter().map(|nb| Ok((nb.id.clone(), self.names(&nb.id)?))).collect::<Result<HashMap<_, _>, String>>()?;
        let mut state = self.state.lock().unwrap();
        state.notebooks.retain(|id, _| snapshots.iter().any(|nb| nb.id == *id));
        let mut list = Vec::new();
        let mut cells = Map::new();
        for nb in &snapshots {
            let notebook = state.notebooks.entry(nb.id.clone()).or_default();
            notebook.path = nb.path.clone();
            let cell = |id: &String| nb.cells.get(id).ok_or_else(|| format!("no cell {id} in {}", nb.id));
            let mut running = Vec::new();
            for id in &nb.order {
                if nb.is_running(cell(id)?) {
                    running.push(id.clone());
                }
            }
            list.push(json!({
                "notebook_id": nb.id, "path": nb.path, "cell_count": nb.order.len(),
                "pending_run": nb.pending_run, "running": running, "execution_allowed": nb.execution_allowed,
            }));
            let pending: HashSet<&str> = nb.pending_run.iter().filter_map(Value::as_str).collect();
            let mut states = Vec::new();
            for id in &nb.order {
                let c = cell(id)?;
                let unrun = pending.contains(id.as_str());
                states.push(json!({
                    "cell_id": id,
                    "running": nb.is_running(c),
                    "errored": c.errored,
                    "unrun": unrun,
                    "author": notebook.author(id, &c.code),
                    "before": notebook.before(id, &c.code, unrun),
                    "version": format!("{:x}", hash(&c.code)),
                    "name": names[&nb.id].get(id),
                }));
            }
            cells.insert(nb.id.clone(), Value::Array(states));
        }
        let idle_stopped: Vec<Value> = state.idle_stopped.iter().map(|(_, entry)| entry.clone()).collect();
        Ok(to_json(&json!({ "notebooks": list, "cells": cells, "idle_stopped": idle_stopped })))
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

    /// A tool call names the notebook it works on: that counts as activity.
    pub fn note_activity(&self, arguments: &Value) {
        let Some(id) = arguments.get("notebook_id").map(julia_string).and_then(|id| parse_uuid(&id)) else { return };
        let now = (self.clock)();
        if let Some(notebook) = self.state.lock().unwrap().notebooks.get_mut(&id) {
            notebook.last_active = Some(now);
        }
    }

    /// Pass a tool call to the engine (`forward`), noting what the core keeps
    /// from it: the agent's edits, and the notebook a session opens first.
    pub fn forward_tool(&self, owner: &str, tool: &str, arguments: &Value, forward: impl FnOnce() -> io::Result<String>) -> io::Result<String> {
        let editing = EDIT_TOOLS.contains(&tool);
        let _editing = editing.then(|| self.editing.lock().unwrap());
        let id = arguments.get("notebook_id").map(julia_string).and_then(|id| parse_uuid(&id));
        let before = id.as_deref().filter(|_| editing).and_then(|id| self.snapshot(id).ok());
        let reply = forward()?;
        let Some(result) = tool_result(&reply) else { return Ok(reply) };
        let code_before = |cell: &str| before.as_ref().and_then(|nb| nb.cells.get(cell)).map_or(String::new(), |c| c.code.clone());
        match (tool, id) {
            ("open_notebook" | "new_notebook", _) => {
                if let Some(path) = result["path"].as_str() {
                    self.opened_by(owner, path);
                }
            }
            ("edit_cell", Some(id)) => {
                if let (Some(cell), Some(code)) = (result["cell_id"].as_str(), result["code"].as_str()) {
                    self.agent_edited(&id, cell, &code_before(cell), code);
                }
            }
            ("add_cell", Some(id)) => {
                if let (Some(cell), Some(code)) = (result["cell_id"].as_str(), result["code"].as_str()) {
                    self.agent_edited(&id, cell, "", code);
                }
            }
            ("edit_cells", Some(id)) => {
                let cells = result["mutation"]["cell_ids"].as_array().into_iter().flatten().filter_map(Value::as_str);
                let codes = arguments["cells"].as_array().into_iter().flatten().filter_map(|edit| edit["code"].as_str());
                for (cell, code) in cells.zip(codes) {
                    self.agent_edited(&id, cell, &code_before(cell), code);
                }
            }
            _ => {}
        }
        Ok(reply)
    }

    /// The agent's tools just wrote this cell's code; `before` is what it replaced.
    fn agent_edited(&self, notebook: &str, cell: &str, before: &str, code: &str) {
        let mut state = self.state.lock().unwrap();
        let notebook = state.notebooks.entry(notebook.to_owned()).or_default();
        notebook.authors.insert(cell.to_owned(), (hash(code), "agent"));
        notebook.befores.entry(cell.to_owned()).or_insert_with(|| before.to_owned());
    }

    /// `keep_notebook_alive`: exempt a notebook from idle stop, or stop exempting it.
    pub fn keep_alive(&self, arguments: &Value) -> Result<Value, String> {
        let id = self.notebook_arg(arguments.get("notebook_id").unwrap_or(&Value::String(String::new())))?;
        let keep = arguments.get("keep").and_then(Value::as_bool).ok_or("ArgumentError: invalid_keep::keep must be true or false")?;
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap();
        let notebook = state.notebooks.entry(id.clone()).or_default();
        notebook.kept_alive = keep;
        notebook.last_active = Some(now);
        Ok(json!({ "notebook_id": id, "kept_alive": keep }))
    }

    /// An open notebook's id from a tool argument, refused as Julia's UUID
    /// parsing and lookup refused it.
    fn notebook_arg(&self, value: &Value) -> Result<String, String> {
        let shown = julia_string(value);
        let invalid = || format!("ArgumentError: invalid_notebook_id::Invalid notebook ID: '{shown}'");
        let id = match value {
            Value::String(s) => parse_uuid(s).ok_or_else(invalid)?,
            Value::Bool(b) => uuid_of(*b as u128),
            Value::Number(n) => match (n.as_u64(), n.as_f64()) {
                (Some(n), _) => uuid_of(n as u128),
                (None, Some(x)) if x >= 0.0 && x.fract() == 0.0 && x < 2f64.powi(128) => uuid_of(x as u128),
                _ => return Err(invalid()),
            },
            _ => return Err(invalid()),
        };
        let known = self.state.lock().unwrap().notebooks.contains_key(&id);
        if known || self.snapshot(&id).is_ok() {
            return Ok(id);
        }
        Err(format!("KeyError: key \"notebook_not_found::No notebook with id '{shown}' in the current session\" not found"))
    }

    /// The app binds a session to its notebook; an empty path clears it.
    pub fn bind(&self, owner: &str, path: &str) {
        let mut state = self.state.lock().unwrap();
        if path.is_empty() {
            state.bindings.remove(owner);
        } else {
            state.bindings.insert(owner.to_owned(), canonical_path(path).unwrap_or_else(|_| path.to_owned()));
        }
    }

    pub fn bound(&self, owner: &str) -> Option<String> {
        self.state.lock().unwrap().bindings.get(owner).cloned()
    }

    /// After a successful open or new: bind the session if it isn't bound yet.
    fn opened_by(&self, owner: &str, path: &str) {
        if !owner.is_empty() {
            let path = canonical_path(path).unwrap_or_else(|_| path.to_owned());
            self.state.lock().unwrap().bindings.entry(owner.to_owned()).or_insert(path);
        }
    }

    /// Why a session may not make this call: it works on another notebook.
    /// Other notebooks stay readable.
    pub fn refusal(&self, owner: &str, tool: &str, arguments: &Value) -> Option<String> {
        if owner.is_empty() {
            return None;
        }
        let bound = self.bound(owner)?;
        let refuse = |what: &str| {
            Some(format!(
                "ArgumentError: one_notebook::This session works on one notebook, {bound}, so it can't {what}. \
                 You can still read other notebooks as plain .jl files. \
                 To work on another notebook, suggest the user start a new session with it."
            ))
        };
        if tool == "open_notebook" || tool == "new_notebook" {
            let requested = arguments.get("path").and_then(Value::as_str);
            if let Some(requested) = requested {
                match canonical_path(requested) {
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

    #[cfg(test)]
    fn idle_stopped(&self) -> Vec<Value> {
        self.state.lock().unwrap().idle_stopped.iter().map(|(_, entry)| entry.clone()).collect()
    }
}

/// A successful tool call's result, from the engine's JSON-RPC reply.
fn tool_result(reply: &str) -> Option<Value> {
    let reply: Value = serde_json::from_str(reply).ok()?;
    if reply["result"]["isError"] != false {
        return None;
    }
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str()?).ok()
}

/// FNV-1a: the app only compares versions with each other.
fn hash(code: &str) -> u64 {
    code.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// A UUID in its only form Julia parses (36 characters, hex and hyphens), lowercased.
fn parse_uuid(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let hyphen = |i: usize| matches!(i, 8 | 13 | 18 | 23);
    let valid = bytes.len() == 36 && bytes.iter().enumerate().all(|(i, b)| if hyphen(i) { *b == b'-' } else { b.is_ascii_hexdigit() });
    valid.then(|| text.to_ascii_lowercase())
}

fn uuid_of(value: u128) -> String {
    let hex = format!("{value:032x}");
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// A notebook path as Julia's runtime compared them: absolute, `~` expanded,
/// and resolved through symlinks as far as it exists.
pub fn canonical_path(path: &str) -> Result<String, String> {
    let expanded = match path.strip_prefix('~') {
        None => path.to_owned(),
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home()),
        Some(_) => return Err("ArgumentError: ~user tilde expansion not yet implemented".into()),
    };
    let absolute = if expanded.starts_with('/') {
        normpath(&expanded)
    } else {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        normpath(&format!("{}/{expanded}", cwd.display()))
    };
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

#[cfg(test)]
mod tests;
