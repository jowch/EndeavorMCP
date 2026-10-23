//! The core's notebook tools and state against a fake engine: an adapter that
//! keeps notebooks in memory, analyses `name = expression` cells, and runs a
//! cell by stamping it with the test's clock.

use std::path::MAIN_SEPARATOR as SEP;

use super::tools::tool_json;
use super::*;

const X: &str = "11111111-1111-1111-1111-111111111111";
const Y: &str = "22222222-2222-2222-2222-222222222222";
const NB: &str = "aaaaaaaa-0000-0000-0000-000000000001";

#[derive(Clone)]
struct FakeCell {
    id: String,
    code: String,
    folded: bool,
    running: bool,
    queued: bool,
    errored: bool,
    last_run: f64,
    output: String,
    hidden: bool,
    stale: bool,
    not_run: bool,
    /// Ember's disabled cells and their dependents, which never run.
    off: bool,
    /// Ember's notes on its last run, "kind::text".
    notes: Vec<String>,
    /// The prompt it waits on, as Ember's `readline()` and `menu()` do: `ask()` and `choose()` run until answered.
    prompt: Option<Value>,
}

impl FakeCell {
    fn new(id: &str, code: &str) -> FakeCell {
        FakeCell { id: id.into(), code: code.into(), folded: false, running: false, queued: false, errored: false, last_run: 0.0, output: String::new(), hidden: false, stale: false, not_run: false, off: false, notes: Vec::new(), prompt: None }
    }

    /// What `a, b = 1, 2` defines, and the names the rest of the code uses.
    fn analysis(&self) -> (Vec<String>, Vec<String>) {
        let code: Vec<&str> = self.code.lines().map(|line| line.split('#').next().unwrap_or_default()).collect();
        let code = code.join("\n");
        let (lhs, rhs) = code.split_once(" = ").unwrap_or(("", &code));
        let names = |text: &str| {
            let mut names: Vec<String> = text.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| w.starts_with(|c: char| c.is_alphabetic())).map(str::to_owned).collect();
            names.sort();
            names.dedup();
            names
        };
        (names(lhs), names(rhs))
    }
}

struct FakeNotebook {
    id: String,
    path: String,
    cells: Vec<FakeCell>,
    safe_preview: bool,
    /// Its process ended by itself, while these cells ran.
    exited: Option<Vec<String>>,
    /// Its snapshot can't be read.
    broken: bool,
    /// The package step under way, as the runtime's `package_step` reports it.
    packages: Option<Value>,
    /// Out of safe preview, its new process still starting (`starting` in Pluto).
    starting: bool,
    /// Cells the next run runs before its own, not naming them in its reply, as
    /// Ember runs an ancestor whose code changed since it last ran.
    runs_first: Vec<String>,
    /// Cells the next run reruns besides its graph dependents, naming them in its reply's
    /// `dependents`, as Ember reruns the readers of a name a cell dropped (#228).
    reruns_besides: Vec<String>,
    /// Ember's lazy mode: a run marks its dependents stale instead of running them.
    lazy: bool,
    /// Autorun as the engines do it: a run reruns its dependents that have run
    /// before, after its own cells. Off unless a test turns it on.
    autorun: bool,
    /// An R notebook's snapshot date, and the date of its ready preview of a move.
    package_date: Option<String>,
    previewed: Option<String>,
}

#[derive(Default)]
struct Engine {
    notebooks: Mutex<Vec<FakeNotebook>>,
    clock: Arc<Mutex<f64>>,
    shut_down: Mutex<Vec<String>>,
    /// The next `run` says it has started, then waits here before it runs,
    /// like Pluto busy with a long run.
    hold: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    /// The next read of every notebook reads them, says it has, then waits
    /// here before it answers, like a snapshot on its way while an edit lands.
    hold_snapshot: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    /// Advanced by each change, as the engine numbers its notifications.
    seq: Mutex<u64>,
    calls: Mutex<Vec<String>>,
    made: Mutex<u32>,
    /// The `timeout` of each waited `run`.
    waits: Mutex<Vec<f64>>,
    /// How many runs went on after their `run` answered: each is named `run-<n>` in its reply.
    watched: Mutex<u32>,
    /// The next `open`, `new` or `allow_execution` waits here before it answers, like a cold
    /// first open compiling; while an `allow_execution` waits, so does a `snapshot`, as the
    /// runtime answers nothing else while it starts the notebook.
    hold_open: Mutex<Option<Receiver<()>>>,
    starting: Mutex<()>,
}

impl Engine {
    fn open(&self, id: &str, path: &str, cells: &[(&str, &str)]) {
        let cells = cells.iter().map(|(id, code)| FakeCell::new(id, code)).collect();
        self.notebooks.lock().unwrap().push(FakeNotebook { id: id.into(), path: path.into(), cells, safe_preview: false, exited: None, broken: false, packages: None, starting: false, runs_first: Vec::new(), reruns_besides: Vec::new(), lazy: false, autorun: false, package_date: None, previewed: None });
    }

    fn with<T>(&self, id: &str, f: impl FnOnce(&mut FakeNotebook) -> T) -> T {
        f(self.notebooks.lock().unwrap().iter_mut().find(|nb| nb.id == id).unwrap())
    }

    fn code(&self, id: &str, cell: &str) -> String {
        self.with(id, |nb| nb.cells.iter().find(|c| c.id == cell).unwrap().code.clone())
    }

    fn order(&self, id: &str) -> Vec<String> {
        self.with(id, |nb| nb.cells.iter().map(|c| c.id.clone()).collect())
    }

    fn snapshot(nb: &FakeNotebook) -> Value {
        if nb.broken {
            return json!({ "notebook_id": nb.id, "path": nb.path });
        }
        json!({
            "notebook_id": nb.id, "path": nb.path,
            "process_status": if nb.safe_preview { "waiting_for_permission" } else if nb.exited.is_some() { "no_process" } else if nb.starting { "starting" } else { "ready" },
            "execution_allowed": !nb.safe_preview && nb.exited.is_none(), "safe_preview": nb.safe_preview, "exited": nb.exited,
            "packages": nb.packages, "lazy": nb.lazy, "package_date": nb.package_date,
            "cell_order": nb.cells.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            "cells": nb.cells.iter().map(|c| json!({
                "cell_id": c.id, "code": c.code, "folded": c.folded, "running": c.running, "queued": c.queued, "errored": c.errored,
                "last_run": c.last_run, "runtime": 0, "output": c.output, "error": if c.errored { json!({ "msg": c.output }) } else { Value::Null }, "hidden": c.hidden, "markdown": c.code.starts_with("md\"") || c.code.starts_with("#'"),
                "stale": c.stale, "not_run": c.not_run, "off": c.off, "notes": c.notes, "prompt": c.prompt,
            })).collect::<Vec<_>>(),
        })
    }

    /// What `using A, B` loads.
    fn packages(code: &str) -> Vec<String> {
        let mut packages: Vec<String> = code.strip_prefix("using ").into_iter().flat_map(|names| names.split(", ")).map(str::to_owned).collect();
        packages.sort();
        packages
    }

    fn graph(nb: &FakeNotebook, params: &Value) -> Value {
        let analysed: Vec<_> = nb.cells.iter().map(|c| (c.id.clone(), c.analysis())).collect();
        let meets = |a: &[String], b: &[String]| a.iter().any(|x| b.contains(x));
        let cells: Vec<Value> = analysed
            .iter()
            .map(|(id, (defs, refs))| {
                let upstream: Vec<&String> = analysed.iter().filter(|(_, (d, _))| meets(d, refs)).map(|(id, _)| id).collect();
                let downstream: Vec<&String> = analysed.iter().filter(|(_, (_, r))| meets(defs, r)).map(|(id, _)| id).collect();
                let mut node = json!({ "cell_id": id, "definitions": defs, "functions": [], "references": refs, "upstream": upstream, "downstream": downstream });
                if params["packages"] == true {
                    node["packages"] = json!(Engine::packages(&nb.cells.iter().find(|c| c.id == *id).unwrap().code));
                }
                node
            })
            .collect();
        json!({ "cells": cells, "order": nb.cells.iter().map(|c| c.id.clone()).collect::<Vec<_>>(), "errable": [] })
    }

    fn adapter_call(&self, method: &str, params: &Value) -> Result<Value, String> {
        self.calls.lock().unwrap().push(method.to_owned());
        if method == "run"
            && let Some((started, release)) = self.hold.lock().unwrap().take()
        {
            started.send(()).unwrap();
            release.recv().unwrap();
        }
        if matches!(method, "open" | "new" | "allow_execution")
            && let Some(release) = self.hold_open.lock().unwrap().take()
        {
            // As the runtime does, a new notebook's file is saved before the slow open.
            if method == "new"
                && let Some(path) = params["path"].as_str()
            {
                std::fs::write(path, "").unwrap();
            }
            let _busy = (method == "allow_execution").then(|| self.starting.lock().unwrap());
            let _ = release.recv();
        }
        if method == "snapshot" {
            drop(self.starting.lock().unwrap());
        }
        let now = *self.clock.lock().unwrap();
        let mut notebooks = self.notebooks.lock().unwrap();
        match method {
            "status" => {
                let listed: Vec<Value> = notebooks.iter().map(|nb| json!({ "notebook_id": nb.id, "path": nb.path, "cell_count": nb.cells.len() })).collect();
                return Ok(json!({ "pluto": "running", "notebooks": listed }));
            }
            "open" | "new" => {
                let path = params["path"].as_str().map_or_else(|| format!("{}{SEP}made.jl", params["folder"].as_str().unwrap_or("/n")), str::to_owned);
                if method == "open" && notebooks.iter().any(|nb| nb.path == path) {
                    return Err(format!("ArgumentError: notebook_already_open::Somebody has {path} open already."));
                }
                let mut made = self.made.lock().unwrap();
                *made += 1;
                let id = format!("cccccccc-0000-0000-0000-{:012}", *made);
                let cells = if method == "new" { vec![FakeCell::new(&format!("dddddddd-0000-0000-0000-{:012}", *made), "")] } else { Vec::new() };
                let listed: Vec<Value> = cells.iter().map(|c| json!({ "cell_id": c.id, "code": c.code })).collect();
                // A file named for it is in Ember's lazy mode.
                let lazy = method == "open" && path.contains("lazy");
                notebooks.push(FakeNotebook { id: id.clone(), path: path.clone(), cells, safe_preview: params["run"] == false, exited: None, broken: false, packages: None, starting: false, runs_first: Vec::new(), reruns_besides: Vec::new(), lazy, autorun: false, package_date: None, previewed: None });
                return Ok(json!({ "notebook_id": id, "path": path, "process_status": "starting", "cells": listed, "lazy": lazy }));
            }
            _ => {}
        }
        let seq = *self.seq.lock().unwrap();
        let Some(id) = params["notebook_id"].as_str() else {
            let all = json!({ "notebooks": notebooks.iter().map(Engine::snapshot).collect::<Vec<_>>(), "seq": seq });
            drop(notebooks);
            if let Some((read, release)) = self.hold_snapshot.lock().unwrap().take() {
                read.send(()).unwrap();
                release.recv().unwrap();
            }
            return Ok(all);
        };
        let Some(at) = notebooks.iter().position(|nb| nb.id == id) else {
            return Err(format!("KeyError: key \"notebook_not_found::No notebook with id '{id}' in the current session\" not found"));
        };
        let nb = &mut notebooks[at];
        let find = |nb: &mut FakeNotebook, cell: &Value| nb.cells.iter().position(|c| c.id == cell.as_str().unwrap()).unwrap();
        match method {
            "snapshot" => {
                let mut snapshot = Engine::snapshot(nb);
                snapshot["seq"] = json!(seq);
                Ok(snapshot)
            }
            "graph" => Ok(Engine::graph(nb, params)),
            "restart" => {
                if nb.safe_preview {
                    return Err("ArgumentError: execution_blocked::The notebook is in safe preview; Run notebook starts it".into());
                }
                for cell in &mut nb.cells {
                    cell.last_run = now;
                }
                Ok(json!({ "restarted": true }))
            }
            // Ember's date preview: today ("2026-10-17") is ready with one change, 2000 has no index.
            "preview_packages" => {
                let date = params["date"].as_str().unwrap_or("2026-10-17").to_owned();
                if date.starts_with("2000") {
                    nb.previewed = None;
                    return Ok(json!({ "date": date, "current_date": nb.package_date, "status": "failed", "changes": [],
                        "problems": [{ "kind": "index_unavailable", "package": null, "message": "no package index for 2000-01-01" }], "message": "no index" }));
                }
                // The 19th isn't published yet, so its preview is of the 18th (#247).
                let asked = date.clone();
                let date = if date == "2026-10-19" { "2026-10-18".to_owned() } else { date };
                nb.previewed = Some(date.clone());
                Ok(json!({ "date": date, "asked": asked, "current_date": nb.package_date, "status": "ready", "changes": [{ "name": "dplyr", "from": "1.1.4", "to": "1.2.1", "change": "upgraded" }], "problems": [], "message": null }))
            }
            "set_package_date" => {
                if nb.previewed.as_deref() != params["date"].as_str() {
                    return Err("ArgumentError: no_preview::no ready, current preview for that date".into());
                }
                nb.package_date = nb.previewed.take();
                Ok(json!({ "date": nb.package_date, "changes": [{ "name": "dplyr", "from": "1.1.4", "to": "1.2.1", "change": "upgraded" }], "process_status": if nb.safe_preview { "waiting_for_permission" } else { "ready" } }))
            }
            "move" => {
                let _ = std::fs::rename(&nb.path, params["path"].as_str().unwrap());
                nb.path = params["path"].as_str().unwrap().to_owned();
                Ok(json!({ "path": nb.path }))
            }
            "shutdown" => {
                let nb = notebooks.remove(at);
                self.shut_down.lock().unwrap().push(nb.id);
                Ok(json!({ "safe_preview": nb.safe_preview }))
            }
            "apply" => {
                let ops = params["ops"].as_array().unwrap();
                for op in ops.iter().filter(|op| op["op"] == "set_code" && op.get("expected").is_some()) {
                    let at = find(nb, &op["cell_id"]);
                    if nb.cells[at].code != op["expected"] {
                        return Err(format!("ArgumentError: stale_read::Cell {} changed since last read; call read_cell again", nb.cells[at].id));
                    }
                }
                let mut inserted = Vec::new();
                for op in ops {
                    match op["op"].as_str().unwrap() {
                        "set_code" => {
                            let at = find(nb, &op["cell_id"]);
                            nb.cells[at].code = op["code"].as_str().ok_or("MethodError: Cannot `convert`")?.into();
                        }
                        "insert" => {
                            let mut made = self.made.lock().unwrap();
                            *made += 1;
                            let mut cell = FakeCell::new(&format!("bbbbbbbb-0000-0000-0000-{:012}", *made), &julia_string(&op["code"]));
                            cell.folded = op["folded"] == true;
                            inserted.push(cell.id.clone());
                            nb.cells.insert(op["index"].as_u64().unwrap() as usize, cell);
                        }
                        "delete" => {
                            let at = find(nb, &op["cell_id"]);
                            nb.cells.remove(at);
                        }
                        "move" => {
                            let at = find(nb, &op["cell_id"]);
                            let cell = nb.cells.remove(at);
                            nb.cells.insert(op["index"].as_u64().unwrap() as usize, cell);
                        }
                        "fold" => {
                            let at = find(nb, &op["cell_id"]);
                            nb.cells[at].folded = op["folded"] == true;
                        }
                        other => panic!("op {other}"),
                    }
                }
                let mut seq = self.seq.lock().unwrap();
                *seq += 1;
                Ok(json!({ "inserted": inserted, "seq": *seq }))
            }
            "run" => {
                if nb.safe_preview {
                    return Ok(json!({ "accepted": false, "process_status": "waiting_for_permission" }));
                }
                if nb.exited.is_some() {
                    return Ok(json!({ "accepted": false, "process_status": "no_process" }));
                }
                let cells: Vec<String> = params["cells"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_owned()).collect();
                // `crash()` takes the notebook's process down while it runs.
                if let Some(cell) = nb.cells.iter().find(|c| cells.contains(&c.id) && c.code.contains("crash()")) {
                    nb.exited = Some(vec![cell.id.clone()]);
                    let mut reply = json!({ "accepted": true, "process_status": "ready" });
                    if params["wait"] == true {
                        reply = json!({ "accepted": true, "process_status": "no_process", "completed": cells, "timed_out": [], "exited": [cell.id] });
                    }
                    return Ok(reply);
                }
                // `linger()` keeps running: a waited run returns with it unfinished, and so
                // with a cell using it that depends on a cell of the run.
                let defined: Vec<String> = nb.cells.iter().filter(|c| cells.contains(&c.id)).flat_map(|c| c.analysis().0).collect();
                let dependents: Vec<String> = nb.cells.iter().filter(|c| !cells.contains(&c.id) && c.code.contains("linger()") && c.analysis().1.iter().any(|name| defined.contains(name))).map(|c| c.id.clone()).collect();
                let asks = |code: &str| code.contains("ask()") || code.contains("choose()");
                let (mut lingering, finishing): (Vec<String>, Vec<String>) = cells.iter().cloned().partition(|id| nb.cells.iter().any(|c| c.id == *id && (c.code.contains("linger()") || asks(&c.code))));
                lingering.extend(dependents);
                for cell in nb.cells.iter_mut().filter(|c| lingering.contains(&c.id)) {
                    cell.running = true;
                    if cell.code.contains("ask()") {
                        cell.prompt = Some(json!({ "id": 1, "text": "Name? ", "choices": [] }));
                    } else if cell.code.contains("choose()") {
                        cell.prompt = Some(json!({ "id": 2, "text": "Selection: ", "choices": ["Yes", "No"] }));
                    }
                }
                let first = std::mem::take(&mut nb.runs_first);
                for cell in nb.cells.iter_mut().filter(|c| finishing.contains(&c.id) || first.contains(&c.id)) {
                    cell.last_run = now;
                    cell.errored = cell.code.contains("error(");
                    cell.output = if cell.errored { "boom".into() } else { format!("ran {}", cell.code) };
                }
                if nb.lazy {
                    for cell in nb.cells.iter_mut().filter(|c| !cells.contains(&c.id) && c.analysis().1.iter().any(|name| defined.contains(name))) {
                        cell.stale = true;
                    }
                } else if nb.autorun {
                    let mut names = defined.clone();
                    for cell in nb.cells.iter_mut().filter(|c| !cells.contains(&c.id) && !c.not_run && !c.off && c.last_run > 0.0) {
                        let (defs, refs) = cell.analysis();
                        if refs.iter().any(|name| names.contains(name)) {
                            names.extend(defs);
                            cell.last_run = now;
                            cell.errored = cell.code.contains("error(");
                            cell.output = if cell.errored { "boom".into() } else { format!("ran {}", cell.code) };
                        }
                    }
                }
                let besides = std::mem::take(&mut nb.reruns_besides);
                for cell in nb.cells.iter_mut().filter(|c| besides.contains(&c.id)) {
                    cell.last_run = now;
                    cell.errored = cell.code.contains("error(");
                    cell.output = if cell.errored { "boom".into() } else { format!("ran {}", cell.code) };
                }
                let mut reply = json!({ "accepted": true, "process_status": "ready" });
                if !besides.is_empty() {
                    reply["dependents"] = json!(besides);
                }
                if params["wait"] == true {
                    self.waits.lock().unwrap().push(params["timeout"].as_f64().unwrap());
                    reply["completed"] = json!(finishing);
                    reply["timed_out"] = json!(lingering);
                }
                // A run that goes on is watched, as the runtime's are.
                if (params["wait"] != true && !cells.is_empty()) || !lingering.is_empty() {
                    let mut watched = self.watched.lock().unwrap();
                    *watched += 1;
                    reply["run"] = json!(format!("run-{watched}"));
                }
                Ok(reply)
            }
            "allow_execution" => {
                if nb.starting {
                    return Err("ArgumentError: execution_not_gated::Notebook is not in safe preview (process_status=starting)".into());
                }
                if !nb.safe_preview {
                    return Ok(json!({ "already_allowed": true, "ran": false, "process_status": "ready" }));
                }
                nb.safe_preview = false;
                nb.starting = params["run"] == true;
                let mut reply = json!({ "already_allowed": false, "ran": params["run"], "process_status": if params["run"] == true { "starting" } else { "ready" } });
                // The run of every cell goes on while a cell in `linger()` runs, and is named, as the runtime's are.
                if params["run"] == true && nb.cells.iter().any(|c| c.code.contains("linger()")) {
                    nb.cells.iter_mut().filter(|c| c.code.contains("linger()")).for_each(|c| c.running = true);
                    let mut watched = self.watched.lock().unwrap();
                    *watched += 1;
                    reply["run"] = json!(format!("run-{watched}"));
                }
                Ok(reply)
            }
            "render_text" => Ok(json!({ "text": if { let at = find(nb, &params["cell_id"]); nb.cells[at].code.contains("table") } { json!("n\tmean\n3\t2.5") } else { Value::Null } })),
            "render_png" => Ok({
                let at = find(nb, &params["cell_id"]);
                let code = &nb.cells[at].code;
                if code.contains("widget") {
                    json!({ "png": "iVBORw==", "mime": "text/html", "source": "embedded image" })
                } else if code.contains("html") {
                    json!({ "png": Value::Null, "mime": "text/html" })
                } else {
                    json!({ "png": if code.contains("plot") { json!("iVBORw==") } else { Value::Null }, "mime": "text/plain" })
                }
            }),
            "validate" => Ok(json!({ "errors": if params["code"].as_str().unwrap().contains('\n') { json!([{ "type": "pluto_multi_expression" }]) } else { json!([]) } })),
            // The running cells end interrupted, but for one in `stubborn()`, and the queued ones don't run.
            // A run queued behind `starting()` waits for the notebook's process, which is still starting:
            // the run is told to skip its cells, which stay queued until the process is up (#179).
            "interrupt" => {
                if nb.cells.iter().any(|c| c.queued && c.code.contains("starting()")) {
                    return Ok(json!({ "interrupted": true, "pending": true }));
                }
                let busy = nb.cells.iter().any(|c| c.running);
                for cell in nb.cells.iter_mut() {
                    cell.queued = false;
                    if cell.running && !cell.code.contains("stubborn()") {
                        (cell.running, cell.errored, cell.output, cell.prompt) = (false, true, "InterruptException".into(), None);
                    }
                }
                Ok(json!({ "interrupted": busy }))
            }
            // Ember's answer_input(): the cell finishes with the answer.
            "answer" => {
                let at = find(nb, &params["cell_id"]);
                let cell = &mut nb.cells[at];
                if cell.prompt.take().is_none() {
                    return Err(format!("ArgumentError: no_prompt::Cell {} isn't waiting for input", cell.id));
                }
                (cell.running, cell.last_run, cell.output) = (false, now, format!("answered {}", params["text"].as_str().unwrap()));
                Ok(json!({ "answered": true }))
            }
            other => Err(format!("ArgumentError: unknown_method::Unknown adapter method: '{other}'")),
        }
    }
}

impl Upstream for Engine {
    fn adapter(&self, raw: &[u8]) -> io::Result<String> {
        let message: Value = serde_json::from_slice(raw).unwrap();
        let reply = match self.adapter_call(message["method"].as_str().unwrap(), &message["params"]) {
            Ok(result) => json!({ "result": result }),
            Err(error) => json!({ "error": error }),
        };
        Ok(reply.to_string())
    }

    fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>> {
        Err(io::ErrorKind::NotConnected.into())
    }
}

struct Setup {
    engine: Arc<Engine>,
    notebooks: Arc<Notebooks>,
    clock: Arc<Mutex<f64>>,
}

fn setup() -> Setup {
    let clock = Arc::new(Mutex::new(1.0e6));
    let engine = Arc::new(Engine { clock: clock.clone(), ..Default::default() });
    let now = clock.clone();
    let notebooks = Arc::new(Notebooks::new(Some(engine.clone()), Box::new(move || *now.lock().unwrap())));
    Setup { engine, notebooks, clock }
}

impl Setup {
    fn hours(&self, hours: f64) {
        self.seconds(hours * 3600.0);
    }

    fn seconds(&self, seconds: f64) {
        *self.clock.lock().unwrap() += seconds;
    }

    /// A tool call as the core makes it for `owner`: its result or error text.
    fn call(&self, owner: &str, tool: &str, arguments: Value) -> Result<Value, String> {
        if let Some(refusal) = self.notebooks.refusal(owner, tool, &arguments, &Folder::Process) {
            return Err(refusal);
        }
        if tool == "keep_notebook_alive" {
            return self.notebooks.keep_alive(&arguments);
        }
        self.notebooks.note_activity(&arguments);
        self.notebooks.tool(owner, tool, &arguments, &Folder::Process, Instant::now()).map(tool_json)
    }

    /// The kind of error a call failed with (`read_required`, ...).
    fn refused(&self, owner: &str, tool: &str, arguments: Value) -> String {
        let error = self.call(owner, tool, arguments).expect_err("refused");
        let text: Value = serde_json::from_str(crate::mcp::tool_error(&error, false)["content"][0]["text"].as_str().unwrap()).unwrap();
        text["error"].as_str().unwrap().to_owned()
    }

    fn read(&self, owner: &str, nb: &str, cell: &str) {
        self.call(owner, "read_cell", json!({ "notebook_id": nb, "cell_id": cell })).unwrap();
    }

    fn edit(&self, owner: &str, nb: &str, cell: &str, code: &str) -> Value {
        self.call(owner, "edit_cell", json!({ "notebook_id": nb, "cell_id": cell, "code": code })).unwrap()
    }
}

/// The next event on `rx`, parsed, within a second.
fn next(rx: &Receiver<String>) -> Value {
    serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).expect("an event")).unwrap()
}

fn cell<'a>(event: &'a Value, notebook: &str, cell: &str) -> &'a Value {
    event["cells"][notebook].as_array().unwrap().iter().find(|c| c["cell_id"] == cell).unwrap()
}

#[test]
fn events_say_who_changed_each_cell_and_what_the_agent_replaced() {
    let s = setup();
    let (first, rx) = s.notebooks.subscribe().unwrap();
    assert_eq!(first, r#"{"asks":[],"cells":{},"engine_ended":[],"idle_stopped":[],"notebooks":[]}"#, "the current state, on connect");

    s.engine.open(NB, "/n/a.jl", &[(X, "x = 6"), (Y, "y = x * 7")]);
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!(
        event["notebooks"],
        json!([{ "notebook_id": NB, "path": "/n/a.jl", "cell_count": 2, "pending_run": [], "running": [], "execution_allowed": true, "this_session": false }])
    );
    assert_eq!(
        cell(&event, NB, Y),
        &json!({ "cell_id": Y, "running": false, "errored": false, "unrun": false, "author": null, "before": null, "version": format!("{:x}", hash("y = x * 7")), "name": "y" })
    );
    s.notebooks.publish();
    assert!(rx.try_recv().is_err(), "nothing changed, nothing sent");

    // An agent edit: unrun, authored by the agent, with the code it replaced.
    s.read("7", NB, Y);
    s.edit("7", NB, Y, "y = x * 8");
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!((&cell(&event, NB, Y)["author"], &cell(&event, NB, Y)["before"], &cell(&event, NB, Y)["unrun"]), (&json!("agent"), &json!("y = x * 7"), &json!(true)));
    assert_eq!(cell(&event, NB, Y)["version"], format!("{:x}", hash("y = x * 8")));
    assert_eq!(event["notebooks"][0]["pending_run"], json!([Y]));

    // A later edit keeps the first before-text; once the cell runs it's forgotten.
    s.edit("7", NB, Y, "y = x * 9");
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, Y)["before"], "y = x * 7");
    s.call("7", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!((&cell(&event, NB, Y)["author"], &cell(&event, NB, Y)["before"], &cell(&event, NB, Y)["unrun"]), (&json!("agent"), &Value::Null, &json!(false)));
    s.seconds(1.0);
    s.edit("7", NB, Y, "y = x * 10");
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, Y)["before"], "y = x * 9", "a new before-text after the run");

    // A change the tools didn't make (Pluto's editor submitting code): the user's.
    s.engine.with(NB, |nb| nb.cells[0].code = "x = 5".into());
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, X)["author"], "user");

    // Seen as each state comes: an edit undone before the next event still counts.
    let cells = |code: &str| json!({ "method": "cell_state", "params": { "notebook_id": NB, "cells": [{ "cell_id": Y, "code": code }] } });
    assert!(s.notebooks.notified(&cells("y = 1")));
    assert!(s.notebooks.notified(&cells("y = x * 10")));
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, Y)["author"], "user");

    // A cell the agent adds: its before-text is empty. Several edits at once.
    let z = s.call("7", "add_cell", json!({ "notebook_id": NB, "after_cell_id": Y, "code": "z = y + 1" })).unwrap()["cell_id"].as_str().unwrap().to_owned();
    s.read("7", NB, X);
    s.call("7", "edit_cells", json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 1" }] })).unwrap();
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!(cell(&event, NB, &z), &json!({ "cell_id": z, "running": false, "errored": false, "unrun": true, "author": "agent", "before": "", "version": format!("{:x}", hash("z = y + 1")), "name": "z" }));
    assert_eq!((&cell(&event, NB, X)["author"], &cell(&event, NB, X)["before"]), (&json!("agent"), &json!("x = 5")));

    // Queued cells count as running only while something runs; names join the first three definitions.
    s.engine.with(NB, |nb| {
        nb.cells[0].queued = true;
        nb.cells[1].code = "f, b, a, c = 1, 2, 3, 4".into();
    });
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!((&event["notebooks"][0]["running"], &cell(&event, NB, Y)["name"]), (&json!([]), &json!("a, b, c")));
    s.engine.with(NB, |nb| nb.cells[1].running = true);
    s.notebooks.publish();
    assert_eq!(next(&rx)["notebooks"][0]["running"], json!([X, Y]));
}

#[test]
fn a_read_of_the_notebook_from_before_an_agent_edit_doesnt_make_it_the_users() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 6"), (Y, "y = x * 7")]);
    let (_, rx) = s.notebooks.subscribe().unwrap();
    s.read("7", NB, X);

    // The app's event is being put together from a read of the notebook taken
    // just before the agent's edit lands.
    let (read_tx, read) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *s.engine.hold_snapshot.lock().unwrap() = Some((read_tx, release_rx));
    let publishing = std::thread::spawn({
        let notebooks = s.notebooks.clone();
        move || notebooks.publish()
    });
    read.recv().unwrap();
    s.edit("7", NB, X, "x = 7");
    release.send(()).unwrap();
    publishing.join().unwrap();
    s.notebooks.publish();
    let mut last = next(&rx);
    while let Ok(event) = rx.try_recv() {
        last = serde_json::from_str(&event).unwrap();
    }
    assert_eq!((&cell(&last, NB, X)["author"], &cell(&last, NB, X)["version"]), (&json!("agent"), &json!(format!("{:x}", hash("x = 7")))));

    // A notification sent before the edit (numbered below it) says nothing
    // new either; one after it with other code is the user's change.
    let seq = *s.engine.seq.lock().unwrap();
    let state = |code: &str, seq: u64| json!({ "method": "cell_state", "seq": seq, "params": { "notebook_id": NB, "cells": [{ "cell_id": X, "code": code }] } });
    s.notebooks.notified(&state("x = 6", seq - 1));
    s.notebooks.publish();
    assert!(rx.try_recv().is_err(), "still the agent's");
    s.notebooks.notified(&state("x = 8", seq + 1));
    s.engine.with(NB, |nb| nb.cells[0].code = "x = 8".into());
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, X)["author"], "user");
}

#[test]
fn edits_are_staged_until_they_run_however_they_run() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x")]);
    s.read("", NB, X);
    let receipt = s.edit("", NB, X, "x = 10");
    assert_eq!(receipt["execution"]["status"], "staged");
    assert_eq!((&receipt["pending_run"], &receipt["stale"], &receipt["affected_cells"]), (&json!([X]), &json!(true), &json!([])));
    assert_eq!(receipt["mutation"], json!({ "type": "edit_cell", "cell_id": X }));
    // The order isn't in a change's receipt; the cell is, without the code the agent sent.
    assert_eq!((&receipt["cell_order"], &receipt["execution_order"], &receipt["code"]), (&Value::Null, &Value::Null, &Value::Null));
    assert_eq!(s.engine.code(NB, X), "x = 10");
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([X]), "listing counts as no read");

    // Nothing pending: nothing to do.
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["pending_run"], &ran["execution"]["status"]), (&json!([X]), &json!([]), &json!("completed")));
    assert_eq!(ran["outputs"]["changed"], json!([{ "cell_id": X, "output_summary": "ran x = 10" }]));
    let noop = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&noop["affected_cells"], &noop["execution"]["status"]), (&json!([]), &json!("completed")));

    // Naming cells: only staged ones, unless forced.
    assert_eq!(
        s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [Y] })),
        Err(format!("ArgumentError: not_staged::Cell {Y} is not in pending_run; stage first or pass force=true"))
    );
    let forced = s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [Y], "force": true, "wait_for_completion": true })).unwrap();
    assert_eq!(forced["affected_cells"], json!([Y]));
    assert_eq!(s.refused("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": ["bad"] })), "invalid_cell_id");

    // Not waited for: running until the engine says the run finished; a run from anywhere counts.
    s.seconds(1.0);
    s.edit("", NB, X, "x = 11");
    let receipt = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!(receipt["execution"]["status"], "running");
    assert_eq!(receipt["warnings"], json!(["async_execution::cells running; read them for the result"]));
    // An edit made once the cell has finished, before the engine says the run ended, ran in
    // no run: the run's end leaves it staged (#231).
    s.seconds(1.0);
    s.edit("", NB, X, "x = 12");
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "cells": [X] } }));
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([X]));
    s.seconds(1.0);
    s.edit("", NB, X, "x = 13");
    s.seconds(1.0);
    s.engine.with(NB, |nb| nb.cells[0].last_run = 1.0e6 + 3.5);
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([]), "Pluto's own run button");
}

#[test]
fn an_edit_whose_cell_is_still_running_gives_the_old_output_as_previous() {
    // edit_cell's run isn't waited for, so the receipt comes back while the
    // cell runs, holding the output from before the edit (#190).
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1")]);
    s.engine.with(NB, |nb| (nb.cells[0].output, nb.cells[0].errored) = ("boom".into(), true));
    s.read("", NB, X);
    let receipt = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = linger()", "run_after": true })).unwrap();
    assert_eq!((&receipt["running"], &receipt["previous_output"], receipt.get("output")), (&json!(true), &json!("boom"), None), "{receipt}");
    assert_eq!((&receipt["previous_errored"], &receipt["previous_error"], receipt.get("errored"), receipt.get("error")), (&json!(true), &json!({ "msg": "boom" }), None, None), "the old error isn't the edit's: {receipt}");
    assert_eq!(receipt["outputs"]["changed"], json!([]), "its old output isn't a change");

    // Once it has finished, a read gives the new output as `output`.
    s.engine.with(NB, |nb| (nb.cells[0].running, nb.cells[0].output, nb.cells[0].errored) = (false, "2".into(), false));
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["output"], &read["errored"], read.get("error"), read.get("previous_output")), (&json!("2"), &json!(false), None, None));
}

#[test]
fn a_run_in_lazy_mode_says_so_and_names_the_stale_cells() {
    // Ember's lazy mode marks a run's dependents stale instead of rerunning
    // them; the agent is told, not left to find them (#196).
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1")]);
    s.engine.with(NB, |nb| nb.lazy = true);
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["on_cell_change"], "lazy");
    s.read("", NB, X);
    s.edit("", NB, X, "x = 2");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["stale_cells"]), (&json!([X]), &json!([Y])), "{ran}");
    assert!(ran["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().starts_with("lazy_mode::")), "{ran}");

    // In autorun there's no such warning or list.
    s.engine.with(NB, |nb| (nb.lazy, nb.cells[1].stale) = (false, false));
    s.edit("", NB, X, "x = 3");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert!(ran.get("stale_cells").is_none() && !ran["warnings"].to_string().contains("lazy_mode"), "{ran}");
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0].get("on_cell_change"), None);
}

#[test]
fn a_run_names_the_dependents_it_reruns_and_a_waited_one_reports_them() {
    // Autorun reruns a run's dependents after its cells. A waited run lists
    // the ones that reran as its own, so a dependent that still fails makes it
    // `errored`; a run not waited for names the ones it will rerun (#192).
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1"), ("33333333-3333-3333-3333-333333333333", "z = error(y)"), ("44444444-4444-4444-4444-444444444444", "w = y"), ("55555555-5555-5555-5555-555555555555", "v = 2")]);
    let (z, w, v) = ("33333333-3333-3333-3333-333333333333", "44444444-4444-4444-4444-444444444444", "55555555-5555-5555-5555-555555555555");
    s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.engine.with(NB, |nb| {
        nb.autorun = true;
        // w hasn't run since the notebook started, so Ember's autorun skips it.
        nb.cells[3].not_run = true;
    });
    s.seconds(1.0);
    s.read("", NB, X);
    s.edit("", NB, X, "x = 2");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["affected_cells"], json!([X, Y, z]), "{ran}");
    assert_eq!(ran["execution"]["status"], "errored", "a dependent that failed fails the run: {ran}");
    let changed: Vec<&str> = ran["outputs"]["changed"].as_array().unwrap().iter().map(|c| c["cell_id"].as_str().unwrap()).collect();
    assert_eq!(changed, [X, Y, z], "{ran}");
    assert_eq!(ran["execution"].get("will_run"), None, "{ran}");

    // Not waited for: the result names what will follow, not v (no dependent) or w (never ran).
    s.seconds(1.0);
    let edited = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 3", "run_after": true })).unwrap();
    assert_eq!(edited["execution"]["will_run"], json!([Y, z]), "{edited}");
    assert!(!edited["execution"]["will_run"].to_string().contains(v) && !edited["execution"]["will_run"].to_string().contains(w));

    // In lazy mode nothing follows: the dependents go stale.
    s.engine.with(NB, |nb| nb.lazy = true);
    s.seconds(1.0);
    let edited = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 4", "run_after": true })).unwrap();
    assert_eq!(edited["execution"].get("will_run"), None, "{edited}");
}

#[test]
fn a_disabled_dependent_or_a_dependent_of_one_is_not_named_as_what_will_run() {
    // Ember never runs a disabled cell, or a cell downstream of one (its
    // `graph$off`), though the graph still links them. A run doesn't name them
    // as what will follow, and a read of one doesn't wait for the run (#232).
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1"), ("33333333-3333-3333-3333-333333333333", "z = y + 1"), ("44444444-4444-4444-4444-444444444444", "w = x + 1")]);
    let (z, w) = ("33333333-3333-3333-3333-333333333333", "44444444-4444-4444-4444-444444444444");
    s.call("7", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    // Not autorun, so the fake doesn't rerun w at once: Ember queues it once x's result is in.
    // The calls are a session's ("7"), whose own run a read waits for.
    s.engine.with(NB, |nb| {
        // Y disabled, and z off because it reads Y.
        nb.cells[1].off = true;
        nb.cells[2].off = true;
    });
    s.seconds(1.0);
    s.read("7", NB, X);
    // linger() keeps the run going, so a read that waited for it would take the read's whole cap.
    let edited = s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = linger()", "run_after": true })).unwrap();
    assert_eq!(edited["execution"]["will_run"], json!([w]), "{edited}");
    let read = |cell: &str, began: Instant| super::tools::tool_json(s.notebooks.tool("7", "read_cell", &json!({ "notebook_id": NB, "cell_id": cell }), &Folder::Process, began).unwrap());
    // A read that waits for the run, which doesn't end, runs out and says so in a `message`.
    let began = Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    let off = read(z, began);
    assert!(off["output"] == "ran z = y + 1" && off.get("message").is_none(), "a read of an off cell doesn't wait for the run: {off}");
    assert!(read(w, began).get("message").is_some(), "a read of the dependent that will run waits for it");
}

#[test]
fn lazy_mode_doesnt_list_an_off_cell_as_stale_to_run() {
    // Ember marks a disabled cell's readers stale like any other, but it won't
    // run them while they're off, so `stale_cells` doesn't ask for it (#232).
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1"), ("33333333-3333-3333-3333-333333333333", "z = x + 2")]);
    let z = "33333333-3333-3333-3333-333333333333";
    s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.engine.with(NB, |nb| {
        nb.lazy = true;
        nb.cells[1].off = true;
    });
    s.seconds(1.0);
    s.read("", NB, X);
    let edited = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 2", "run_after": true })).unwrap();
    assert_eq!(edited["stale_cells"], json!([z]), "{edited}");
}

#[test]
fn a_dependent_the_engine_reruns_beyond_the_graph_is_named_and_reported() {
    // A rename: Y read the name X dropped, so the graph no longer links them, but
    // Ember reruns Y, and the runtime names it in the run's `dependents` (#228).
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = error(x)")]);
    s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.engine.with(NB, |nb| nb.autorun = true);
    s.seconds(1.0);
    s.read("", NB, X);
    s.edit("", NB, X, "b = 1");
    s.engine.with(NB, |nb| nb.reruns_besides = vec![Y.into()]);
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["affected_cells"], json!([X, Y]), "{ran}");
    assert_eq!(ran["execution"]["status"], "errored", "{ran}");

    s.seconds(1.0);
    s.engine.with(NB, |nb| nb.reruns_besides = vec![Y.into()]);
    let edited = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "c = 1", "run_after": true })).unwrap();
    assert_eq!(edited["execution"]["will_run"], json!([Y]), "{edited}");
}

#[test]
fn a_cells_notes_reach_the_agent_in_the_runs_warnings_and_in_read_cell() {
    // Ember's page shows notes under a cell, such as a value another cell
    // defines changed in place, whose readers don't rerun; the agent sees
    // them too (#207).
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1")]);
    let note = "changed_in_place::Changed x in place, which another cell defines, so the cells that read it don't rerun. Make the change in the cell that defines it.";
    s.engine.with(NB, |nb| nb.cells[1].notes = vec![note.into()]);
    s.read("", NB, Y);
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true })).unwrap();
    let wanted = format!("changed_in_place::Cell {Y}: Changed x in place, which another cell defines, so the cells that read it don't rerun. Make the change in the cell that defines it.");
    assert!(ran["warnings"].as_array().unwrap().iter().any(|w| *w == wanted), "{ran}");
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(read["notes"], json!([note]), "{read}");

    // A cell with none says nothing.
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    assert!(!ran["warnings"].to_string().contains("changed_in_place"), "{ran}");
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap().get("notes"), None);
}

#[test]
fn an_edit_the_user_didnt_let_run_is_kept_staged() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1")]);
    let runs = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "run").count();
    s.read("7", NB, X);
    let before = runs();
    let edit = json!({ "notebook_id": NB, "cell_id": X, "code": "x = 2", "run_after": true });
    let receipt = tool_json(s.notebooks.tool_unrun("7", "edit_cell", &edit, &Folder::Process, Instant::now()).unwrap());
    assert_eq!(s.engine.code(NB, X), "x = 2", "the edit is made");
    assert_eq!(runs(), before, "and not run");
    assert_eq!((&receipt["execution"]["status"], &receipt["pending_run"]), (&json!("staged"), &json!([X])));
    assert_eq!(receipt["warnings"], json!(["not_approved::The user chose not to run this yet. The edit is kept, staged and not run."]));

    let add = json!({ "notebook_id": NB, "after_cell_id": X, "code": "y = x", "run_after": true });
    let receipt = tool_json(s.notebooks.tool_unrun("7", "add_cell", &add, &Folder::Process, Instant::now()).unwrap());
    assert_eq!((runs(), s.engine.order(NB).len()), (before, 2));
    assert_eq!(receipt["warnings"][0], "not_approved::The user chose not to run this yet. The edit is kept, staged and not run.");
}

#[test]
fn an_approved_run_of_cells_the_users_run_already_reached_runs_nothing_again() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x")]);
    let runs = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "run").count();
    // The user's run (Pluto's, after their own change) reaches what the agent staged.
    let users_run = |s: &Setup| {
        s.seconds(1.0);
        let now = *s.clock.lock().unwrap();
        s.engine.with(NB, |nb| nb.cells.iter_mut().for_each(|c| c.last_run = now));
    };
    s.read("", NB, Y);
    s.edit("", NB, Y, "y = x + 1");
    users_run(&s);
    let before = runs();
    let receipt = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before, "not run again");
    assert_eq!((&receipt["affected_cells"], &receipt["execution"]["status"]), (&json!([Y]), &json!("completed")));
    assert_eq!(receipt["warnings"], json!([format!("already_ran::{Y} already ran after the user's change; not run again, so it ran once.")]));
    assert_eq!(receipt["mutation"], json!({ "type": "execute_cell", "cell_id": Y }));

    // Asked again, it runs: the agent means it.
    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before + 1);
    // A cell the tools didn't change runs as asked, ran or not.
    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!(runs(), before + 2);

    // submit_changes, naming the cell or not.
    s.edit("", NB, Y, "y = x + 2");
    users_run(&s);
    let named = s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [Y] })).unwrap();
    assert_eq!((&named["affected_cells"], runs()), (&json!([Y]), before + 2));
    s.edit("", NB, Y, "y = x + 3");
    users_run(&s);
    let all = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&all["affected_cells"], runs()), (&json!([Y]), before + 2));
    assert!(all["warnings"][0].as_str().unwrap().starts_with("already_ran::"));
    let noop = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&noop["affected_cells"], &noop["warnings"]), (&json!([]), &json!([])), "said once");

    // Only when every cell ran: one still unrun, and both run.
    s.read("", NB, X);
    s.edit("", NB, X, "x = 2");
    s.edit("", NB, Y, "y = x + 4");
    users_run(&s);
    s.seconds(1.0);
    s.edit("", NB, X, "x = 3");
    s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [X, Y], "force": true })).unwrap();
    assert_eq!(runs(), before + 3);

    // The user's run still under way is waited for.
    s.edit("", NB, Y, "y = x + 5");
    s.seconds(1.0);
    s.engine.with(NB, |nb| nb.cells[1].queued = true);
    let engine = s.engine.clone();
    let now = *s.clock.lock().unwrap();
    let finish = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        engine.with(NB, |nb| {
            nb.cells[1].queued = false;
            nb.cells[1].last_run = now;
        });
    });
    let waited = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    finish.join().unwrap();
    assert_eq!((&waited["affected_cells"], runs()), (&json!([Y]), before + 3));
}

#[test]
fn an_approved_run_after_run_anyway_runs_nothing_again_edited_or_not() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x")]);
    let runs = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "run").count();
    let last_run = |s: &Setup| s.engine.with(NB, |nb| nb.cells[1].last_run);
    let users_run = |s: &Setup| {
        s.seconds(1.0);
        let now = *s.clock.lock().unwrap();
        s.engine.with(NB, |nb| nb.cells.iter_mut().for_each(|c| c.last_run = now));
    };
    let run_anyway = |s: &Setup, before: f64| s.notebooks.run_anyway(NB, &json!([{ "cell_id": Y, "last_run": before }])).unwrap();
    let before = runs();

    // Y, which the tools never changed, ran in the user's run: not again.
    let was = last_run(&s);
    users_run(&s);
    run_anyway(&s, was);
    let receipt = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before, "not run again");
    assert_eq!(receipt["warnings"], json!([format!("already_ran::{Y} already ran after the user's change; not run again, so it ran once.")]));
    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before + 1, "asked again, it runs");

    // The user's run never reached it: it runs.
    run_anyway(&s, last_run(&s));
    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before + 2);

    // Said long before the call: it runs.
    let was = last_run(&s);
    run_anyway(&s, was);
    users_run(&s);
    s.seconds(120.0);
    s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [Y], "force": true })).unwrap();
    assert_eq!(runs(), before + 3);
}

#[test]
fn reads_come_before_edits_and_edits_of_several_cells_are_all_or_nothing() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = 2")]);
    let edit = |code: &str| json!({ "notebook_id": NB, "cell_id": X, "code": code });
    assert_eq!(
        s.call("", "edit_cell", edit("x = 2")),
        Err(format!("ArgumentError: read_required::Call read_cell or read_notebook_code before editing cell {X}"))
    );
    s.read("", NB, X);
    s.call("", "edit_cell", edit("x = 2")).unwrap();
    s.call("", "edit_cell", edit("x = 3")).unwrap();
    s.engine.with(NB, |nb| nb.cells[0].code = "x = 99".into());
    assert_eq!(s.call("", "edit_cell", edit("x = 4")), Err(format!("ArgumentError: stale_read::Cell {X} changed since last read; call read_cell again")));
    // A read after someone else's change sends the code: it isn't what this session has.
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"]), (&json!("x = 99"), &Value::Null));
    s.call("", "read_notebook_code", json!({ "notebook_id": NB })).unwrap();
    s.call("", "edit_cell", edit("x = 4")).unwrap();
    assert_eq!(s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X })), Err("KeyError: key \"code\" not found".into()));
    assert_eq!(s.refused("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 5", "run_after": "yes" })), "invalid_argument");

    // Changed between the core's look and the engine's change: the engine refuses.
    s.engine.with(NB, |nb| nb.cells[1].code = "y = 3".into());
    s.read("", NB, Y);
    let edits = json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 10" }, { "cell_id": Y, "code": "y = 20" }] });
    s.engine.with(NB, |nb| nb.cells[1].code = "y = 4".into());
    assert_eq!(s.refused("", "edit_cells", edits.clone()), "stale_read");
    assert_eq!((s.engine.code(NB, X), s.engine.code(NB, Y)), ("x = 4".into(), "y = 4".into()));
    s.read("", NB, Y);
    let receipt = s.call("", "edit_cells", edits).unwrap();
    assert_eq!(receipt["mutation"], json!({ "type": "edit_cells", "cell_ids": [X, Y], "code_folded": { X: false, Y: false } }));
    assert_eq!((&receipt["pending_run"], &receipt["execution"]["status"]), (&json!([X, Y]), &json!("staged")));

    // Each edit can fold or unfold its cell; one without `folded` leaves the fold as it is.
    let folded = || s.engine.with(NB, |nb| (nb.cells[0].folded, nb.cells[1].folded));
    let bad = json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 11", "folded": true }, { "cell_id": Y, "code": "y = 21", "folded": "yes" }] });
    assert_eq!(s.refused("", "edit_cells", bad), "invalid_argument");
    assert_eq!((s.engine.code(NB, X), folded()), ("x = 10".into(), (false, false)), "a refused batch changes no cell");
    s.engine.with(NB, |nb| nb.cells[1].folded = true);
    let receipt = s.call("", "edit_cells", json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 11", "folded": true }, { "cell_id": Y, "code": "y = 21" }] })).unwrap();
    assert_eq!((s.engine.code(NB, X), s.engine.code(NB, Y), folded()), ("x = 11".into(), "y = 21".into(), (true, true)));
    // The receipt says each cell's fold, the one left as it was too.
    assert_eq!(receipt["mutation"]["code_folded"], json!({ X: true, Y: true }));
    s.call("", "edit_cells", json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 12", "folded": false }, { "cell_id": Y, "code": "y = 22", "folded": false }] })).unwrap();
    assert_eq!(folded(), (false, false));
    s.call("", "edit_cells", json!({ "notebook_id": NB, "cells": [{ "cell_id": Y, "code": "y = 23", "folded": null }] })).unwrap();
    assert_eq!(folded(), (false, false));
}

#[test]
fn adding_moving_folding_and_deleting_cells() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[]);
    let add = |args: Value| s.call("", "add_cell", args);
    let first = add(json!({ "notebook_id": NB, "code": "a = 1" })).unwrap();
    let a = first["cell_id"].as_str().unwrap().to_owned();
    assert_eq!((&first["code"], &first["code_folded"], &first["pending_run"]), (&Value::Null, &json!(false), &json!([a])), "not the code the agent just sent");
    // Read at once, the cell's code is what the agent wrote, so it isn't sent again unless asked for.
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"], &read["code_sha"]), (&Value::Null, &json!(true), &json!("b5bc1ffd9091")));
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a, "include_code": true })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"]), (&json!("a = 1"), &Value::Null));
    // Another session hasn't read it, so it gets the code.
    assert_eq!(s.call("9", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap()["code"], "a = 1");
    // A read more than ten of the session's calls ago may have left the agent's context: the code comes again.
    for _ in 0..11 {
        s.call("", "list_notebooks", json!({})).unwrap();
    }
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"]), (&json!("a = 1"), &Value::Null));
    // A read that left the code out doesn't count as sending it: reads every few calls don't keep it out for good.
    let calls = |n| for _ in 0..n {
        s.call("", "list_notebooks", json!({})).unwrap();
    };
    calls(7);
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap()["code_unchanged"], true);
    calls(7);
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"]), (&json!("a = 1"), &Value::Null));
    assert_eq!(s.refused("", "add_cell", json!({ "notebook_id": NB, "code": "b = a" })), "placement_required");
    assert_eq!(s.refused("", "add_cell", json!({ "notebook_id": NB, "code": "b = a", "after_cell_id": "" })), "placement_required");
    assert_eq!(s.refused("", "add_cell", json!({ "notebook_id": NB, "code": "b = a", "after_cell_id": a, "folded": "yes" })), "invalid_argument");
    // The agent read the cell it added: it can add after it, and edit it at once.
    let b = add(json!({ "notebook_id": NB, "code": "b = a", "after_cell_id": a, "folded": true })).unwrap();
    assert_eq!((&b["code_folded"], &b["execution"]["status"]), (&json!(true), &json!("staged")));
    let b = b["cell_id"].as_str().unwrap().to_owned();
    let c = add(json!({ "notebook_id": NB, "code": 42, "after_cell_id": a, "run_after": true })).unwrap();
    assert_eq!((&c["code"], &c["affected_cells"], &c["execution"]["status"]), (&json!("42"), &json!([c["cell_id"]]), &json!("running")), "the code as the engine has it, since it isn't what was sent");
    let c = c["cell_id"].as_str().unwrap().to_owned();
    assert_eq!(s.engine.code(NB, &c), "42");
    assert_eq!(s.engine.order(NB), [a.clone(), c.clone(), b.clone()]);
    s.edit("", NB, &b, "b = a + 1");
    // An edit can fold or unfold the cell too, and leaves it as it is without `folded`.
    let folded_b = || s.engine.with(NB, |nb| nb.cells.iter().find(|c| c.id == b).unwrap().folded);
    assert_eq!(s.refused("", "edit_cell", json!({ "notebook_id": NB, "cell_id": b, "code": "b = a + 2", "folded": "no" })), "invalid_argument");
    assert_eq!((s.engine.code(NB, &b), folded_b()), ("b = a + 1".to_owned(), true), "a refused edit changes nothing");
    let unfolded = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": b, "code": "b = a + 2", "folded": false })).unwrap();
    assert_eq!((&unfolded["code_folded"], &unfolded["execution"]["status"]), (&json!(false), &json!("staged")));
    assert_eq!((s.engine.code(NB, &b), folded_b()), ("b = a + 2".to_owned(), false));
    s.edit("", NB, &b, "b = a + 1");
    assert!(!folded_b());

    let moved = s.call("", "move_cell", json!({ "notebook_id": NB, "cell_id": b, "after_cell_id": "" })).unwrap();
    assert_eq!((&moved["mutation"]["old_index"], &moved["mutation"]["new_index"], &moved["cell_order"]), (&json!(3), &json!(1), &json!([b, a, c])));
    s.call("", "move_cell", json!({ "notebook_id": NB, "cell_id": b, "after_cell_id": c })).unwrap();
    assert_eq!(s.engine.order(NB), [a.clone(), c.clone(), b.clone()]);
    assert_eq!(
        s.call("", "move_cell", json!({ "notebook_id": NB, "cell_id": b, "after_cell_id": b })),
        Err(format!("KeyError: key \"cell_not_found::Target cell '{b}' not found\" not found"))
    );
    assert_eq!(s.refused("", "move_cell", json!({ "notebook_id": NB, "cell_id": b, "after_cell_id": "bad" })), "invalid_cell_id");

    let folded = s.call("", "fold_cell", json!({ "notebook_id": NB, "cell_id": a, "folded": true })).unwrap();
    assert_eq!((&folded["mutation"], &folded["execution"]["status"]), (&json!({ "type": "fold_cell", "cell_id": a, "folded": true }), &json!("completed")));
    assert!(s.engine.with(NB, |nb| nb.cells[0].folded));
    for bad in [json!("true"), json!(1), Value::Null] {
        assert_eq!(s.refused("", "fold_cell", json!({ "notebook_id": NB, "cell_id": a, "folded": bad })), "invalid_argument");
    }

    let deleted = s.call("", "delete_cell", json!({ "notebook_id": NB, "cell_id": b })).unwrap();
    assert_eq!(deleted["warnings"], json!(["async_execution::cell deletion cleanup queued"]));
    assert_eq!((&deleted["execution"]["status"], &deleted["pending_run"]), (&json!("completed"), &json!([a])));
    assert_eq!(s.engine.order(NB), [a.clone(), c.clone()]);
    assert_eq!(*s.engine.calls.lock().unwrap().iter().rev().nth(1).unwrap(), "run", "Pluto's cleanup after a delete");
    assert_eq!(s.refused("", "delete_cell", json!({ "notebook_id": NB, "cell_id": b })), "cell_not_found");
}

/// An Ember notebook reads with its own file's markers, and its text cells, `#'` lines already,
/// with no `# md:`.
#[test]
fn an_ember_notebook_reads_as_code_with_its_own_markers() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "#' # Title"), (Y, "b <- 2")]);
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("", "list_notebooks", json!({})).unwrap();
    let read = |markdown: bool| s.call("", "read_notebook_code", json!({ "notebook_id": R, "order": "visual", "include_markdown": markdown })).unwrap();
    assert_eq!(read(false)["code"], format!("# %% id={Y}\nb <- 2"));
    assert_eq!(read(true)["code"], format!("# %% id={X}\n#' # Title\n\n# %% id={Y}\nb <- 2"));
}

/// A hand-written R notebook keeps its short cell ids (`# %% id=B`), and the tools take them
/// (Ember #82). Any other id that isn't a UUID is refused as before, a short one the notebook lacks too.
#[test]
fn an_ember_notebooks_short_cell_ids_name_its_cells() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[("A", "a <- 1"), ("B", "b <- a")]);
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("", "list_notebooks", json!({})).unwrap();
    s.read("", R, "A");
    s.read("", R, "B");
    s.edit("", R, "B", "b <- a + 1");
    assert_eq!(ember.code(R, "B"), "b <- a + 1");
    s.call("", "move_cell", json!({ "notebook_id": R, "cell_id": "B", "after_cell_id": "" })).unwrap();
    assert_eq!(ember.order(R), ["B".to_owned(), "A".to_owned()]);
    s.call("", "move_cell", json!({ "notebook_id": R, "cell_id": "B", "after_cell_id": "A" })).unwrap();
    assert_eq!(ember.order(R), ["A".to_owned(), "B".to_owned()]);
    let c = s.call("", "add_cell", json!({ "notebook_id": R, "code": "c <- 3", "after_cell_id": "A" })).unwrap();
    assert_eq!(ember.order(R)[1], c["cell_id"].as_str().unwrap());
    s.call("", "submit_changes", json!({ "notebook_id": R, "cell_ids": ["B"] })).unwrap();
    assert_eq!(s.refused("", "read_cell", json!({ "notebook_id": R, "cell_id": "Z" })), "invalid_cell_id");
    assert_eq!(s.refused("", "read_cell", json!({ "notebook_id": R, "cell_id": "cell:1" })), "invalid_cell_id");
}

#[test]
fn restoring_a_turns_cells_puts_their_code_back_all_or_nothing() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x")]);
    let (_, rx) = s.notebooks.subscribe().unwrap();
    let restore = |cells: Value| s.notebooks.restore_cells(NB, &cells);
    let pending = || s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"].clone();

    // A turn edits x without running it and adds z: restoring puts x back, in step with its last run, and deletes z.
    s.read("7", NB, X);
    s.edit("7", NB, X, "x = 2");
    s.read("7", NB, Y);
    let z = s.call("7", "add_cell", json!({ "notebook_id": NB, "after_cell_id": Y, "code": "z = y" })).unwrap()["cell_id"].as_str().unwrap().to_owned();
    let calls = s.engine.calls.lock().unwrap().len();
    let restored = restore(json!([{ "cell_id": X, "expected": "x = 2", "code": "x = 1" }, { "cell_id": z, "expected": "z = y", "code": null }])).unwrap();
    assert_eq!(restored, json!({ "restored": [X, z], "changed": [] }));
    assert_eq!((s.engine.order(NB), s.engine.code(NB, X)), (vec![X.to_owned(), Y.to_owned()], "x = 1".to_owned()));
    assert_eq!(pending(), json!([]), "x's code is what last ran");
    assert_eq!(s.engine.calls.lock().unwrap()[calls..].iter().filter(|m| *m == "run").count(), 1, "only Pluto's cleanup of the deleted cell");
    let event = rx.try_iter().last().map(|e| serde_json::from_str::<Value>(&e).unwrap()).expect("an event");
    assert_eq!((&cell(&event, NB, X)["author"], &cell(&event, NB, X)["before"], &cell(&event, NB, X)["unrun"]), (&json!("user"), &Value::Null, &json!(false)));
    assert_eq!(s.refused("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 3" })), "stale_read", "the agent reads it again first");

    // A turn that ran its edit: the restored code hasn't run, so the cell is unrun.
    s.seconds(1.0);
    s.read("7", NB, X);
    s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 4", "run_after": true })).unwrap();
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "cells": [X] } }));
    s.seconds(1.0);
    assert_eq!(pending(), json!([]));
    assert_eq!(restore(json!([{ "cell_id": X, "expected": "x = 4", "code": "x = 1" }])).unwrap()["restored"], json!([X]));
    assert_eq!(pending(), json!([X]));
    s.notebooks.publish();
    let event = rx.try_iter().last().map(|e| serde_json::from_str::<Value>(&e).unwrap()).expect("an event");
    assert_eq!((&cell(&event, NB, X)["unrun"], &cell(&event, NB, X)["before"]), (&json!(true), &json!("x = 4")), "the turn's code ran, so the diff is against it");
    // The agent hasn't read what was put back: its run waits until it has.
    let refused = s.call("7", "submit_changes", json!({ "notebook_id": NB }));
    assert!(refused.as_ref().is_err_and(|e| e.contains(&format!("run_conflict::{X} changed since this session last read them"))), "{refused:?}");
    s.read("7", NB, X);
    s.call("7", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!(pending(), json!([]));
    s.notebooks.publish();

    // Two turns leave x unrun (1, then 2, then 3): restoring the second keeps it unrun, with the diff against 1.
    s.seconds(1.0);
    s.edit("7", NB, X, "x = 2");
    s.edit("7", NB, X, "x = 3");
    restore(json!([{ "cell_id": X, "expected": "x = 3", "code": "x = 2" }])).unwrap();
    assert_eq!(pending(), json!([X]));
    s.notebooks.publish();
    let event = rx.try_iter().last().map(|e| serde_json::from_str::<Value>(&e).unwrap()).expect("an event");
    assert_eq!(cell(&event, NB, X)["before"], "x = 1");
    s.read("7", NB, X);
    s.call("7", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();

    // The user changed one of the cells since, or it's gone: nothing changes, and the reply says which.
    s.read("7", NB, Y);
    s.edit("7", NB, Y, "y = x + 1");
    s.engine.with(NB, |nb| nb.cells[0].code = "x = 5".into());
    assert_eq!(s.engine.code(NB, X), "x = 5");
    let gone = "33333333-3333-3333-3333-333333333333";
    let refused = restore(json!([{ "cell_id": Y, "expected": "y = x + 1", "code": "y = x" }, { "cell_id": X, "expected": "x = 1", "code": "x = 0" }, { "cell_id": gone, "expected": "", "code": null }])).unwrap();
    assert_eq!(refused, json!({ "restored": [], "changed": [X, gone] }));
    assert_eq!((s.engine.code(NB, X), s.engine.code(NB, Y)), ("x = 5".to_owned(), "y = x + 1".to_owned()));

    for bad in [json!([]), json!({}), json!([{ "cell_id": Y, "code": "y = x" }]), json!([{ "cell_id": Y, "expected": "y = x + 1", "code": 1 }]), json!([{ "cell_id": Y, "expected": "y = x + 1" }])] {
        assert_eq!(restore(bad), Err("ArgumentError: invalid_argument::cells must be a list of {cell_id, expected, code}".into()));
    }
    let twice = json!([{ "cell_id": Y, "expected": "y = x + 1", "code": "y = x" }, { "cell_id": Y, "expected": "y = x + 1", "code": null }]);
    assert_eq!(restore(twice), Err(format!("ArgumentError: invalid_argument::cell {Y} is listed twice")));
    assert_eq!(s.engine.code(NB, Y), "y = x + 1");

    // A run of the turn's edit still going when the user restores: what was put back is unrun.
    s.read("7", NB, X);
    s.call("7", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.notebooks.publish();
    s.seconds(1.0);
    s.read("7", NB, Y);
    s.edit("7", NB, Y, "y = x + 2");
    s.engine.with(NB, |nb| nb.cells[1].running = true);
    restore(json!([{ "cell_id": Y, "expected": "y = x + 2", "code": "y = x + 1" }])).unwrap();
    assert_eq!(pending(), json!([Y]));
}

#[test]
fn reading_the_notebook_as_code_hides_boilerplate_and_counts_as_reading_it() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "md\"# Title\""), ("33333333-3333-3333-3333-333333333333", " \n"), ("44444444-4444-4444-4444-444444444444", "PLUTO = 1")]);
    s.engine.with(NB, |nb| nb.cells[3].hidden = true);
    let read = |args: Value| s.call("", "read_notebook_code", args).unwrap();
    let code = read(json!({ "notebook_id": NB }));
    assert_eq!(code["cell_ids"], json!([X, "33333333-3333-3333-3333-333333333333"]));
    assert_eq!(code["code"], format!("# ╔═╡ {X}\nx = 1\n\n# ╔═╡ 33333333-3333-3333-3333-333333333333\n# (empty)"));
    assert_eq!(code["order"], "execution");
    assert_eq!(code["markdown_cells_left_out"], 1);
    let with_markdown = read(json!({ "notebook_id": NB, "order": "visual", "include_markdown": true }));
    assert!(with_markdown.get("markdown_cells_left_out").is_none());
    assert!(with_markdown["code"].as_str().unwrap().contains(&format!("# ╔═╡ {Y}\n# md:\nmd\"# Title\"")));
    assert_eq!(
        s.call("", "read_notebook_code", json!({ "notebook_id": NB, "order": "bogus" })),
        Err("ArgumentError: invalid_order::order must be 'execution' or 'visual', got 'bogus'".into())
    );
    assert_eq!(
        s.call("", "read_notebook_code", json!({ "notebook_id": NB, "order": 5 })),
        Err("TypeError: in keyword argument order, expected AbstractString, got a value of type Int64".into())
    );
    assert_eq!(
        s.call("", "read_notebook_code", json!({ "notebook_id": NB, "include_markdown": "yes" })),
        Err("TypeError: non-boolean (String) used in boolean context".into())
    );
    s.edit("", NB, X, "x = 2");
    let code = read(json!({ "notebook_id": NB }));
    assert_eq!((&code["stale_cell_ids"], &code["pending_run"]), (&json!([X]), &json!([X])));
}

#[test]
fn safe_preview_keeps_edits_staged_until_execution_is_allowed() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + 1")]);
    s.engine.with(NB, |nb| nb.safe_preview = true);
    s.read("", NB, X);
    s.edit("", NB, X, "x = 10");
    let blocked = "execution_blocked::notebook is not running code (process_status=waiting_for_permission); pending_run kept; nothing ran. allow_execution is for when the user asked you to run the notebook";
    for tool in ["submit_changes", "run_all_cells"] {
        let receipt = s.call("", tool, json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
        assert_eq!((&receipt["execution"]["status"], &receipt["warnings"], &receipt["pending_run"]), (&json!("blocked"), &json!([blocked]), &json!([X])));
        assert_eq!(receipt["outputs"]["changed"], json!([]));
    }
    // An edit run straight away isn't staged, even when the run is refused.
    s.read("", NB, Y);
    let receipt = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": Y, "code": "y = x * 2", "run_after": true })).unwrap();
    assert_eq!((&receipt["execution"]["status"], &receipt["affected_cells"], &receipt["pending_run"]), (&json!("blocked"), &json!([Y]), &json!([X])));

    assert_eq!(s.call("", "allow_execution", json!({})), Err("ArgumentError: invalid_notebook_id::notebook_id is required".into()));
    assert_eq!(s.call("", "allow_execution", json!({ "notebook_id": NB, "run_notebook": "no" })), Err("TypeError: non-boolean (String) used in boolean context".into()));
    let allowed = s.call("", "allow_execution", json!({ "notebook_id": NB, "run_notebook": false })).unwrap();
    assert_eq!(allowed, json!({ "notebook_id": NB, "execution_allowed": true, "already_allowed": false, "ran": false, "process_status": "ready" }));
    let again = s.call("", "allow_execution", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&again["already_allowed"], &again.get("run_warnings")), (&json!(true), &None));
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["execution"]["status"], &ran["pending_run"]), (&json!("completed"), &json!([])));
}

#[test]
fn a_waited_run_that_outlasts_the_cap_returns_with_its_cells_still_running() {
    let s = setup();
    let (b, z) = ("44444444-4444-4444-4444-444444444444", "33333333-3333-3333-3333-333333333333");
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + linger()"), (z, "z = y + 1"), (b, "b = 5")]);
    // Another run's cell, running too.
    s.engine.with(NB, |nb| nb.cells[3].running = true);
    s.read("", NB, X);
    s.edit("", NB, X, "x = 10");

    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert!(s.engine.waits.lock().unwrap()[0] <= super::tools::WAIT_SECONDS);
    assert_eq!(ran["execution"], json!({ "status": "running", "still_running": [Y] }), "the cells of this run, not the notebook's other running cell: {ran}");
    assert_eq!(ran["affected_cells"], json!([X]));
    assert_eq!(ran["outputs"]["changed"].as_array().unwrap().iter().map(|c| c["cell_id"].clone()).collect::<Vec<_>>(), vec![json!(X)]);
    assert_eq!(ran["pending_run"], json!([]), "cells being run are no longer staged");
    assert!(ran["warnings"][0].as_str().unwrap().starts_with("execution_timeout::"), "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("Call read_cell on one of them"), "{ran}");

    // A read shows the dependent running and not stale; a bare submit_changes runs nothing again.
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!((&read["running"], &read["stale"]), (&json!(true), &json!(false)));
    let runs = || s.engine.calls.lock().unwrap().iter().filter(|call| *call == "run").count();
    let before = runs();
    let again = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&again["affected_cells"], &again["execution"]["status"], runs()), (&json!([]), &json!("completed"), before));

    // The dependent finishes: the reads say so.
    s.seconds(5.0);
    let now = *s.clock.lock().unwrap();
    s.engine.with(NB, |nb| {
        nb.cells[1].running = false;
        nb.cells[1].last_run = now;
        nb.cells[1].output = "done".into();
    });
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["running"], json!([b]));
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap()["running"], false);

    // A run that finishes in time is as before.
    s.seconds(1.0);
    s.read("", NB, X);
    s.edit("", NB, X, "x = 11");
    s.engine.with(NB, |nb| nb.cells[1].code = "y = x + 1".into());
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["execution"], json!({ "status": "completed" }));
    assert_eq!((ran.get("message"), &ran["warnings"]), (None, &json!([])));
}

#[test]
fn cells_waiting_on_a_package_install_say_so_and_a_read_of_one_waits() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "using DataFrames"), (Y, "y = 1")]);
    let installing = json!({ "step": "precompiling", "packages": ["DataFrames"], "seconds": 130, "last_line": "Precompiling DataFrames..." });
    s.engine.with(NB, |nb| {
        nb.packages = Some(installing.clone());
        nb.cells.iter_mut().for_each(|c| c.queued = true);
    });

    // Queued cells count as running while packages install, and the notebook says what it is doing.
    let listed = s.call("", "list_notebooks", json!({})).unwrap();
    assert_eq!((&listed[0]["running"], &listed[0]["packages"]), (&json!([X, Y]), &installing));

    // A read of a queued cell waits for the install, within the call's cap, then says what is going on.
    let began = Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    let read = super::tools::tool_json(s.notebooks.tool("", "read_cell", &json!({ "notebook_id": NB, "cell_id": Y }), &Folder::Process, began).unwrap());
    assert!(began.elapsed() >= Duration::from_secs(44), "it waited");
    assert!(began.elapsed() < Duration::from_secs(super::tools::WAIT_SECONDS as u64), "within the cap");
    assert_eq!((&read["queued"], &read["packages"]), (&json!(true), &installing));
    let message = read["message"].as_str().unwrap();
    for part in ["Julia is getting DataFrames", "precompiling, about 2 minutes so far", "normal the first time", "call read_cell", "Precompiling DataFrames..."] {
        assert!(message.contains(part), "{part}: {message}");
    }

    // A caller that hangs up during the wait gets no read: the edit after it is refused.
    let gone = s.notebooks.tool_watched("", "read_cell", &json!({ "notebook_id": NB, "cell_id": X }), &Folder::Process, Instant::now(), &|| true);
    assert!(gone.is_err_and(|e| e.contains("client_gone")));
    assert_eq!(s.refused("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "using CSV" })), "read_required");

    // Past the usual time, the agent is told to wait only while the log moves.
    s.engine.with(NB, |nb| nb.packages.as_mut().unwrap()["seconds"] = json!(660));
    let began = Instant::now().checked_sub(Duration::from_secs(44)).expect("a clock that has run that long");
    let late = super::tools::tool_json(s.notebooks.tool("", "read_cell", &json!({ "notebook_id": NB, "cell_id": Y }), &Folder::Process, began).unwrap());
    let message = late["message"].as_str().unwrap();
    for part in ["about 11 minutes", "longer than usual", "Tell the user", "only if that line has changed", "stop waiting"] {
        assert!(message.contains(part), "{part}: {message}");
    }
    s.engine.with(NB, |nb| nb.packages = Some(installing.clone()));

    // A run's receipt while the install goes on says the same.
    s.engine.with(NB, |nb| nb.cells[1].queued = false);
    s.read("", NB, Y);
    s.edit("", NB, Y, "y = 2");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(ran["packages"], installing, "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("DataFrames"), "{ran}");
    s.engine.with(NB, |nb| nb.cells[1].queued = true);

    // The install ends and the cell runs: the waiting read returns then, with the output.
    let engine = s.engine.clone();
    let now = *s.clock.lock().unwrap();
    let finish = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        engine.with(NB, |nb| {
            nb.packages = None;
            for cell in &mut nb.cells {
                cell.queued = false;
                cell.last_run = now;
                cell.output = "done".into();
            }
        });
    });
    let began = Instant::now();
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    finish.join().unwrap();
    assert!(began.elapsed() < Duration::from_secs(5), "returned when the install ended");
    assert_eq!((&read["queued"], &read["output"], read.get("message"), read.get("packages")), (&json!(false), &json!("done"), None, None));
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0].get("packages"), None);

    // A queued cell with no install under way (safe preview's leftover flag) is read at once.
    s.engine.with(NB, |nb| nb.cells[0].queued = true);
    let began = Instant::now();
    s.read("", NB, X);
    assert!(began.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_read_of_a_cell_the_sessions_own_run_is_running_waits_for_it() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "sleep(60)")]);
    let running = |busy: bool| s.engine.with(NB, |nb| nb.cells[0].running = busy);
    let read = |owner: &str, began: Instant| super::tools::tool_json(s.notebooks.tool(owner, "read_cell", &json!({ "notebook_id": NB, "cell_id": X }), &Folder::Process, began).unwrap());
    let late = || Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");

    // The user's own run is read at once.
    running(true);
    let began = Instant::now();
    let at_once = read("7", began);
    assert_eq!(at_once["running"], true);
    assert!(began.elapsed() < Duration::from_secs(1), "the user's run isn't waited for");
    assert_eq!(at_once.get("message"), None, "nothing was waited for, so nothing ran out: {at_once}");

    // The session's own run is waited for, within the call's cap; another session's read isn't held up.
    running(false);
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    running(true);
    let began = Instant::now();
    assert_eq!(read("8", began)["running"], true);
    assert!(began.elapsed() < Duration::from_secs(1), "another session's run isn't waited for");
    let began = late();
    let ran_out = read("7", began);
    assert_eq!(ran_out["running"], true, "still running when the wait ends");
    assert!(ran_out["message"].as_str().is_some_and(|m| m.starts_with("This read waited 4") && m.contains("s, and the cell is still running")), "the read says its wait ran out (#203): {ran_out}");
    assert!(began.elapsed() >= Duration::from_secs(44), "it waited");
    assert!(began.elapsed() < Duration::from_secs(super::tools::WAIT_SECONDS as u64), "within the cap");

    // The run ends: the waiting read returns then, with the output.
    let engine = s.engine.clone();
    let now = *s.clock.lock().unwrap();
    let finish = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        engine.with(NB, |nb| {
            nb.cells[0].running = false;
            nb.cells[0].last_run = now;
            nb.cells[0].output = "done".into();
        });
    });
    let began = Instant::now();
    let done = read("7", began);
    finish.join().unwrap();
    assert!(began.elapsed() < Duration::from_secs(5), "returned when the run ended");
    assert_eq!((&done["running"], &done["output"]), (&json!(false), &json!("done")));

    // Once it has ended, a later run of the cell isn't the session's.
    running(true);
    let began = Instant::now();
    read("7", began);
    assert!(began.elapsed() < Duration::from_secs(1), "a later run isn't waited for");
}

#[test]
fn a_read_of_a_dependent_still_running_in_the_sessions_run_waits_for_it() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + linger()")]);
    let read = |began: Instant| super::tools::tool_json(s.notebooks.tool("7", "read_cell", &json!({ "notebook_id": NB, "cell_id": Y }), &Folder::Process, began).unwrap());
    let late = || Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");

    // A waited run whose own cell finished and whose dependent outlasts the cap, and a run not
    // waited for: either way the dependent is waited for, though the run's own cell is done.
    for wait in [true, false] {
        let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": wait })).unwrap();
        if wait {
            assert_eq!(ran["execution"]["still_running"], json!([Y]), "{ran}");
        }
        let began = late();
        assert_eq!(read(began)["running"], true);
        assert!(began.elapsed() >= Duration::from_secs(44), "waited for the dependent (wait_for_completion {wait})");

        // The run ends: a later run of the cell isn't the session's.
        let run = format!("run-{}", 2 - wait as u8);
        s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": run, "cells": [X] } }));
        let began = Instant::now();
        read(began);
        assert!(began.elapsed() < Duration::from_secs(1), "the run ended (wait_for_completion {wait})");
        s.engine.with(NB, |nb| nb.cells[1].running = false);
    }
}

#[test]
fn a_read_of_a_dependent_the_sessions_run_will_rerun_waits_for_it_before_it_is_queued() {
    // The engine queues a dependent only once its parent's result is in, so
    // right after an edit's run it looks idle with its old output (#192).
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1")]);
    s.call("7", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.seconds(1.0);
    s.read("7", NB, X);
    let edited = s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = linger()", "run_after": true })).unwrap();
    assert_eq!(edited["execution"]["will_run"], json!([Y]), "{edited}");
    let read = |began: Instant| super::tools::tool_json(s.notebooks.tool("7", "read_cell", &json!({ "notebook_id": NB, "cell_id": Y }), &Folder::Process, began).unwrap());
    let began = Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    assert_eq!(read(began)["running"], false);
    assert!(began.elapsed() >= Duration::from_secs(44), "waited for the dependent though it isn't queued yet");

    // Once it has run, or the run has ended, a read doesn't wait.
    s.engine.with(NB, |nb| nb.cells[1].last_run += 1.0);
    let began = Instant::now();
    read(began);
    assert!(began.elapsed() < Duration::from_secs(1), "it ran");
    s.engine.with(NB, |nb| nb.cells[1].last_run -= 1.0);
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-1", "cells": [X] } }));
    s.engine.with(NB, |nb| nb.cells[0].running = false);
    let began = Instant::now();
    read(began);
    assert!(began.elapsed() < Duration::from_secs(1), "the run ended");
}

#[test]
fn a_waited_submit_with_nothing_staged_waits_for_the_sessions_run() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + linger()")]);
    let submit = |began: Instant| super::tools::tool_json(s.notebooks.tool("7", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, began).unwrap());
    let late = || Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");

    // Nothing is staged, but the dependent of the session's run is still running: not "completed" (#151).
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let began = late();
    let receipt = submit(began);
    assert_eq!((&receipt["execution"]["status"], &receipt["execution"]["still_running"]), (&json!("running"), &json!([Y])), "{receipt}");
    assert!(began.elapsed() >= Duration::from_secs(44), "waited for the run");

    // Once the run ends there is nothing to wait for.
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-1", "cells": [X] } }));
    let began = Instant::now();
    assert_eq!(submit(began)["execution"]["status"], "completed");
    assert!(began.elapsed() < Duration::from_secs(1), "nothing to wait for");
    // Nor for the user's run, or another session's.
    let receipt = super::tools::tool_json(s.notebooks.tool("8", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, Instant::now()).unwrap());
    assert_eq!(receipt["execution"]["status"], "completed", "{receipt}");
}

#[test]
fn a_waited_submit_with_nothing_staged_says_how_the_cells_that_ran_meanwhile_went() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + linger()")]);
    let submit = || super::tools::tool_json(s.notebooks.tool("7", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, Instant::now()).unwrap());
    // `y`, the dependent of the session's run, ends while the call waits: it errored, and so the run did (#171).
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let ends = |errored: bool| {
        std::thread::sleep(Duration::from_millis(700));
        let now = *s.clock.lock().unwrap() + 1.0;
        s.engine.with(NB, |nb| {
            let y = &mut nb.cells[1];
            (y.running, y.errored, y.last_run) = (false, errored, now);
            y.output = if errored { "boom".into() } else { "ran".into() };
        });
    };
    let receipt = std::thread::scope(|scope| {
        scope.spawn(|| ends(true));
        submit()
    });
    assert_eq!((&receipt["execution"]["status"], &receipt["affected_cells"]), (&json!("errored"), &json!([Y])), "{receipt}");
    assert_eq!(receipt["outputs"]["changed"][0]["cell_id"], Y, "{receipt}");
    assert_eq!(receipt["execution"].get("still_running"), None, "{receipt}");
    // And when it ends well, "completed", naming it.
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let receipt = std::thread::scope(|scope| {
        scope.spawn(|| ends(false));
        submit()
    });
    assert_eq!((&receipt["execution"]["status"], &receipt["affected_cells"]), (&json!("completed"), &json!([Y])), "{receipt}");
    // A cell queued at the start that ends without running (an interrupt dropped it) isn't one of them.
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.engine.with(NB, |nb| nb.cells[0].queued = true);
    let receipt = std::thread::scope(|scope| {
        scope.spawn(|| {
            ends(false);
            s.engine.with(NB, |nb| nb.cells[0].queued = false);
        });
        submit()
    });
    assert_eq!((&receipt["execution"]["status"], &receipt["affected_cells"]), (&json!("completed"), &json!([Y])), "{receipt}");
}

#[test]
fn allow_executions_run_of_the_notebook_is_the_sessions_run() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + linger()")]);
    s.engine.with(NB, |nb| nb.safe_preview = true);
    s.call("7", "allow_execution", json!({ "notebook_id": NB })).unwrap();
    s.engine.with(NB, |nb| nb.starting = false);
    // A waited submit with nothing staged waits for it, as for a run `submit_changes` starts (#170).
    let began = Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    let receipt = super::tools::tool_json(s.notebooks.tool("7", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, began).unwrap());
    assert_eq!((&receipt["execution"]["status"], &receipt["execution"]["still_running"]), (&json!("running"), &json!([Y])), "{receipt}");
    assert!(began.elapsed() >= Duration::from_secs(44), "waited for the run");
    // Not another session's.
    let receipt = super::tools::tool_json(s.notebooks.tool("8", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, Instant::now()).unwrap());
    assert_eq!(receipt["execution"]["status"], "completed", "{receipt}");
    // Once it ends there is nothing to wait for.
    s.engine.with(NB, |nb| nb.cells[1].running = false);
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-1", "cells": [X, Y] } }));
    let began = Instant::now();
    let receipt = super::tools::tool_json(s.notebooks.tool("7", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, began).unwrap());
    assert_eq!(receipt["execution"]["status"], "completed", "{receipt}");
    assert!(began.elapsed() < Duration::from_secs(1), "nothing to wait for");
}

#[test]
fn a_waited_run_that_outlasts_the_cap_during_an_install_keeps_its_still_running_list() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + linger()")]);
    s.engine.with(NB, |nb| {
        nb.packages = Some(json!({ "step": "installing", "packages": ["CSV"], "seconds": 20, "last_line": null }));
        nb.cells[0].queued = true;
    });
    s.read("", NB, Y);
    s.edit("", NB, Y, "y = x + linger() + 1");
    s.engine.with(NB, |nb| nb.cells[0].queued = true);
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["execution"], json!({ "status": "running", "still_running": [Y] }), "{ran}");
    assert_eq!(ran["packages"]["packages"], json!(["CSV"]));
    let message = ran["message"].as_str().unwrap();
    assert!(message.contains("CSV") && message.contains("Don't run the cells in `execution.still_running` again"), "{message}");
}

#[test]
fn a_cell_that_errored_is_reported_before_the_cells_still_running() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = error(1)"), (Y, "y = linger()")]);
    let ran = s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["execution"], json!({ "status": "errored", "still_running": [Y] }));
    assert_eq!(ran["pending_run"], json!([]));
}

#[test]
fn a_wait_has_a_floor_however_long_the_call_took_to_start() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1")]);
    // The user took 44 seconds, then a minute, to answer the approval card.
    for taken in [44, 60] {
        let began = Instant::now().checked_sub(Duration::from_secs(taken)).expect("a clock that has run that long");
        s.notebooks.tool("", "execute_cell", &json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true }), &Folder::Process, began).unwrap();
        assert_eq!(s.engine.waits.lock().unwrap().last(), Some(&5.0), "after {taken} s");
    }
}

#[test]
fn a_run_also_runs_the_cells_it_needs_that_never_ran() {
    let s = setup();
    let z = "33333333-3333-3333-3333-333333333333";
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + 1"), (z, "z = y * 3")]);
    s.engine.with(NB, |nb| nb.safe_preview = true);
    let preview = s.notebooks.run_preview("allow_execution", &json!({ "notebook_id": NB, "run_notebook": false })).unwrap();
    assert_eq!(preview["count"], 3, "the card counts the notebook whether or not it runs");
    s.call("", "allow_execution", json!({ "notebook_id": NB, "run_notebook": false })).unwrap();

    let run = json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true });
    let preview = s.notebooks.run_preview("execute_cell", &run).unwrap();
    assert_eq!((&preview["needed_ids"], &preview["dependent_ids"]), (&json!([X]), &json!([z])));
    let ran = s.call("", "execute_cell", run.clone()).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["execution"]["status"]), (&json!([X, Y]), &json!("completed")));
    assert_eq!(ran["warnings"], json!([format!("also_ran::Also ran {X}: cells this run needs that had never run.")]));
    assert_eq!(s.notebooks.run_preview("execute_cell", &run).unwrap()["needed_ids"], json!([]), "x has run now");
}

#[test]
fn a_run_lists_a_staged_cell_it_needs_that_the_engine_ran_first() {
    // As Ember does (#187): y needs x, whose staged edit Ember runs first. The
    // receipt lists x, with its new output, and x is no longer staged.
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1")]);
    s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.seconds(1.0);
    s.read("", NB, X);
    s.edit("", NB, X, "x = 5");
    s.seconds(1.0);
    s.engine.with(NB, |nb| nb.runs_first = vec![X.into()]);
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["pending_run"], &ran["execution"]["status"]), (&json!([X, Y]), &json!([]), &json!("completed")), "{ran}");
    let changed: Vec<_> = ran["outputs"]["changed"].as_array().unwrap().iter().map(|c| (c["cell_id"].clone(), c["output_summary"].clone())).collect();
    assert_eq!(changed, vec![(json!(X), json!("ran x = 5")), (json!(Y), json!("ran y = x + 1"))]);
    assert_eq!(ran["warnings"], json!([format!("also_ran::Also ran {X}: cells this run needs that were edited or stale since they last ran.")]));

    // An engine that leaves it staged (Pluto): the receipt is the target's alone.
    s.seconds(1.0);
    s.edit("", NB, X, "x = 6");
    s.seconds(1.0);
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["pending_run"], &ran["warnings"]), (&json!([Y]), &json!([X]), &json!([])), "{ran}");
}

#[test]
fn a_run_forgets_staged_cells_no_longer_in_the_notebook() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = 2")]);
    s.read("", NB, X);
    s.read("", NB, Y);
    s.edit("", NB, X, "x = 10");
    s.edit("", NB, Y, "y = 20");
    // Gone the way Pluto's page or a reload removes a cell, not through the tools.
    s.engine.with(NB, |nb| nb.cells.remove(0));
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([Y, X]), "kept until a run");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["pending_run"]), (&json!([Y]), &json!([])));
    assert_eq!(s.refused("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [X] })), "not_staged");
    assert_eq!(s.refused("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [X], "force": true })), "cell_not_found");
}

#[test]
fn several_sessions_on_one_notebook() {
    let s = setup();
    let (a, b, c) = ("aaaaaaaa-0000-0000-0000-00000000000a", "aaaaaaaa-0000-0000-0000-00000000000b", "aaaaaaaa-0000-0000-0000-00000000000c");
    s.engine.open(NB, "/n/a.jl", &[(a, "a = 1"), (b, "b = a + 1"), (c, "c = 10")]);
    let call = |owner: &str, tool: &str, args: Value| {
        let mut args = args;
        args["notebook_id"] = json!(NB);
        s.call(owner, tool, args)
    };
    for owner in ["A", "B"] {
        call(owner, "read_notebook_code", json!({})).unwrap();
    }
    call("B", "edit_cell", json!({ "cell_id": a, "code": "a = 2" })).unwrap();

    // B's edit doesn't count as A's read of the cell.
    assert_eq!(s.refused("A", "edit_cell", json!({ "notebook_id": NB, "cell_id": a, "code": "a = 3" })), "stale_read");

    call("A", "edit_cell", json!({ "cell_id": c, "code": "c = 11" })).unwrap();

    let conflict = format!(
        "ArgumentError: run_conflict::{a} changed since this session last read them (by the user, by another Endeavor session, or by this conversation before it was resumed), and the cells you're running depend on them. \
         Read them (read_cell or read_notebook_code), then run again."
    );
    assert_eq!(call("A", "execute_cell", json!({ "cell_id": b })), Err(conflict.clone()));
    assert_eq!(call("A", "run_all_cells", json!({})), Err(conflict.clone()));
    assert_eq!(call("A", "submit_changes", json!({})), Err(conflict.clone()), "pending runs include B's staged edit of a");
    let ran = call("A", "submit_changes", json!({ "cell_ids": [c], "wait_for_completion": true })).unwrap();
    assert_eq!(ran["execution"]["status"], "completed");

    // An edit that would run into the conflict is kept but left staged.
    let staged = call("A", "edit_cell", json!({ "cell_id": b, "code": "b = a + 2", "run_after": true })).unwrap();
    assert_eq!((&staged["execution"]["status"], staged["pending_run"].as_array().unwrap().contains(&json!(b))), (&json!("staged"), true));
    let warned = format!("{} The edit is staged, not run.", conflict.strip_prefix("ArgumentError: ").unwrap());
    assert!(staged["warnings"].as_array().unwrap().contains(&json!(warned)));

    s.read("A", NB, a);
    let cleared = call("A", "execute_cell", json!({ "cell_id": b, "wait_for_completion": true })).unwrap();
    assert_eq!(cleared["execution"]["status"], "completed");

    // Calls without an owner are exempt, and their changes aren't another session's.
    s.read("B", NB, a);
    call("B", "edit_cell", json!({ "cell_id": a, "code": "a = 4" })).unwrap();
    call("", "execute_cell", json!({ "cell_id": b })).unwrap();
    s.read("", NB, a);
    call("", "edit_cell", json!({ "cell_id": a, "code": "a = 5" })).unwrap();
    s.seconds(200.0);
    call("A", "execute_cell", json!({ "cell_id": b })).unwrap();
}

#[test]
fn graph_tools_follow_the_engines_analysis() {
    let s = setup();
    let (a, b, c, d) = ("aaaaaaaa-0000-0000-0000-00000000000a", "aaaaaaaa-0000-0000-0000-00000000000b", "aaaaaaaa-0000-0000-0000-00000000000c", "aaaaaaaa-0000-0000-0000-00000000000d");
    s.engine.open(NB, "/n/a.jl", &[(a, "x = 1"), (b, "y = x * 7"), (c, "z = y +\nx"), (d, "w = 2 # mentions x")]);
    let tool = |name: &str, args: Value| {
        let mut args = args;
        args["notebook_id"] = json!(NB);
        s.call("", name, args).unwrap()
    };
    assert_eq!(tool("get_cell_dependencies", json!({ "cell_id": c })), json!({ "upstream": [a, b], "symbols": ["x", "y"] }));
    assert_eq!(tool("get_cell_dependencies", json!({ "cell_id": a })), json!({ "upstream": [], "symbols": [] }));
    assert_eq!(tool("get_cell_dependents", json!({ "cell_id": a })), json!({ "downstream": [b, c] }));
    assert_eq!(tool("find_symbol_definitions", json!({ "symbol": "x" })), json!([{ "cell_id": a, "line_hint": 1 }]));
    assert_eq!(tool("find_symbol_references", json!({ "symbol": "x" })), json!([{ "cell_id": b, "line_hint": 1 }, { "cell_id": c, "line_hint": 2 }]));
    assert_eq!(tool("get_cell_order", json!({})), json!({ "notebook_id": NB, "cell_ids": [a, b, c, d] }));
    assert_eq!(tool("get_execution_order", json!({})), json!({ "notebook_id": NB, "cell_ids": [a, b, c, d] }));
    assert_eq!(tool("validate_cell", json!({ "cell_id": a, "code": "a = 1\nb = 2" })), json!({ "valid": false, "errors": [{ "type": "pluto_multi_expression" }] }));
    assert_eq!(s.call("", "find_symbol_references", json!({ "notebook_id": NB })), Err("KeyError: key \"symbol\" not found".into()));

    let preview = |tool: &str, args: Value| {
        let mut args = args;
        args["notebook_id"] = json!(NB);
        s.notebooks.run_preview(tool, &args).unwrap()
    };
    assert_eq!(
        preview("execute_cell", json!({ "cell_id": a })),
        json!({ "all": false, "count": 1, "cells": [{ "id": a, "name": "x", "code": "x = 1" }], "needed_ids": [], "dependents": 2, "dependent_ids": [b, c], "packages": [] })
    );
    assert_eq!(preview("submit_changes", json!({ "cell_ids": [b] }))["dependents"], 1);
    assert_eq!(preview("run_all_cells", json!({})), json!({ "all": true, "count": 4, "cells": [], "needed_ids": [], "dependents": 0, "dependent_ids": [], "packages": [] }));
    assert_eq!(preview("allow_execution", json!({ "run_notebook": false })), json!({ "all": false, "count": 4, "cells": [], "needed_ids": [], "dependents": 0, "dependent_ids": [], "packages": [] }));
    s.read("", NB, c);
    s.edit("", NB, c, "z = y");
    assert_eq!(preview("submit_changes", json!({}))["cells"], json!([{ "id": c, "name": "z", "code": "z = y" }]));
    assert_eq!(s.notebooks.run_preview("submit_changes", &json!({ "notebook_id": NB, "cell_ids": ["bad"] })), Err("ArgumentError: Malformed UUID string: \"bad\"".into()));
    assert_eq!(s.notebooks.run_preview("execute_cell", &json!({})), Err("KeyError: key \"notebook_id\" not found".into()));
}

#[test]
fn a_whole_notebook_run_names_the_packages_it_loads_in_notebook_order() {
    let s = setup();
    let (a, b, c) = ("aaaaaaaa-0000-0000-0000-00000000000a", "aaaaaaaa-0000-0000-0000-00000000000b", "aaaaaaaa-0000-0000-0000-00000000000c");
    s.engine.open(NB, "/n/a.jl", &[(a, "using Statistics, Dates"), (b, "using LinearAlgebra"), (c, "using Dates")]);
    s.engine.with(NB, |nb| nb.safe_preview = true);
    let packages = |tool: &str, args: Value| s.notebooks.run_preview(tool, &args).unwrap()["packages"].clone();
    assert_eq!(packages("allow_execution", json!({ "notebook_id": NB })), json!(["Dates", "Statistics", "LinearAlgebra"]));
    assert_eq!(packages("run_all_cells", json!({ "notebook_id": NB })), json!(["Dates", "Statistics", "LinearAlgebra"]));
    assert_eq!(packages("execute_cell", json!({ "notebook_id": NB, "cell_id": a })), json!([]), "only a whole-notebook run names them");
}

#[test]
fn search_code_cuts_snippets_as_julia_did() {
    let s = setup();
    let long = format!("{}needle{}", "a".repeat(50), "b".repeat(50));
    let greek = "θ = 0.5 # angle θ in radians, about 28.6° — ok";
    s.engine.open(NB, "/n/a.jl", &[(X, &long), (Y, greek)]);
    let search = |query: &str| s.call("", "search_code", json!({ "notebook_id": NB, "query": query }));
    assert_eq!(search("needle").unwrap(), json!([{ "cell_id": X, "snippet": format!("{}needle{}", "a".repeat(40), "b".repeat(40)) }]));
    assert_eq!(search("θ").unwrap(), json!([{ "cell_id": Y, "snippet": "θ = 0.5 # angle θ in radians, about 28." }]));
    assert_eq!(search("°").unwrap(), json!([{ "cell_id": Y, "snippet": "= 0.5 # angle θ in radians, about 28.6° — ok" }]));
    assert_eq!(search("zzz").unwrap(), json!([]));
    assert_eq!(super::tools::snippet_around(&format!("a{}", "é".repeat(30)), "a"), Err("StringIndexError: invalid index [41], valid nearby indices [40]=>'é', [42]=>'é'".into()));
}

#[test]
fn view_cell_output_sends_the_png_the_engine_renders() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "plot(x)"), (Y, "y = 1")]);
    let view = |cell: &str| s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": NB, "cell_id": cell }), &Folder::Process, Instant::now());
    match view(X).unwrap() {
        Reply::Image { meta, png_base64 } => {
            assert_eq!(meta, json!({ "cell_id": X, "shown_as": "text/plain", "png_bytes": 4 }));
            assert_eq!(png_base64, "iVBORw==");
        }
        Reply::Json(_) => panic!("no image"),
    }
    assert_eq!(view(Y).err(), Some(format!("ArgumentError: no_image::Cell {Y}: its output (text/plain) has no PNG rendering; read_cell shows it as text")));
    s.engine.with(NB, |nb| nb.cells[0].errored = true);
    assert_eq!(view(X).err(), Some(format!("ArgumentError: no_image::Cell {X} errored; read_cell shows the error")));
}

#[test]
fn view_cell_output_says_when_the_picture_came_from_the_html() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "widget(fig)")]);
    match s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": NB, "cell_id": X }), &Folder::Process, Instant::now()).unwrap() {
        Reply::Image { meta, .. } => assert_eq!(meta, json!({ "cell_id": X, "shown_as": "text/html", "png_bytes": 4, "source": "embedded image" })),
        Reply::Json(_) => panic!("no image"),
    }
}

#[test]
fn the_app_s_picture_stands_in_for_html_with_no_png_and_for_a_picture_from_the_html() {
    const Z: &str = "33333333-3333-3333-3333-333333333333";
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "html(fig)"), (Y, "y = 1"), (Z, "widget(fig)")]);
    let result = |cell: &str| {
        let reply = s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": NB, "cell_id": cell }), &Folder::Process, Instant::now());
        match reply {
            Ok(Reply::Image { meta, png_base64 }) => crate::mcp::image_result(&meta, png_base64),
            Ok(Reply::Json(_)) => panic!("not an image"),
            Err(error) => crate::mcp::tool_error(&error, false),
        }
    };
    assert!(super::snapshot_may_help(&result(X)), "HTML with no PNG");
    assert!(super::snapshot_may_help(&result(Z)), "a picture from the HTML");
    assert!(!super::snapshot_may_help(&result(Y)), "text");
    s.engine.with(NB, |nb| nb.cells[0].errored = true);
    assert!(!super::snapshot_may_help(&result(X)), "an error");
}

#[test]
fn read_cell_adds_the_text_form_of_rich_outputs() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "table"), (Y, "y = 1")]);
    s.engine.with(NB, |nb| {
        nb.cells[0].output = "[text/html output, 90 bytes]".into();
        nb.cells[1].output = "1".into();
    });
    let read = |cell: &str| s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": cell })).unwrap();
    assert_eq!((&read(X)["output"], &read(X)["output_text"]), (&json!("[text/html output, 90 bytes]"), &json!("n\tmean\n3\t2.5")));
    assert_eq!(read(Y).get("output_text"), None);
    s.engine.with(NB, |nb| nb.cells[0].errored = true);
    assert_eq!(read(X).get("output_text"), None);
}

#[test]
fn a_run_receipt_has_the_text_form_of_rich_outputs() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "table")]);
    s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["outputs"]["changed"], json!([{ "cell_id": X, "output_summary": "ran table", "output_text": "n\tmean\n3\t2.5" }]));
    assert_eq!(super::tools::cut("abcé", 4), "abc\n… (cut; read_cell shows more)");
}

#[test]
fn an_open_that_outlasts_the_call_goes_on_and_the_same_call_again_gets_its_result() {
    let s = setup();
    let dir = temp_notebooks("slow-open", 1)[0].rsplit_once(SEP).unwrap().0.to_owned();
    let path = format!("{dir}{SEP}slow.jl");
    let (release, held) = mpsc::channel();
    *s.engine.hold_open.lock().unwrap() = Some(held);
    // A call that already spent its 45 s (on the user's answer, say) waits only the least a run is waited for.
    let late = Instant::now() - Duration::from_secs(60);
    let make = |began| s.notebooks.tool("a", "new_notebook", &json!({ "path": path }), &Folder::Process, began).map(tool_json);
    let started = Instant::now();
    let said = make(late).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(said.starts_with(&format!("notebook_opening::Creating '{path}' is taking longer than one call waits")), "{said}");
    assert_eq!(crate::mcp::tool_error(&said, false)["isError"], false, "still opening is not a failure");

    // The creation went on; the same call again gets it, rather than "already exists".
    release.send(()).unwrap();
    let made = make(Instant::now()).unwrap();
    assert_eq!((&made["path"], &made["created"]), (&json!(path), &json!(true)), "{made}");
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|m| *m == "new").count(), 1, "made once");
    // Its result was collected: a third call is a new call, and the file is there.
    assert!(make(Instant::now()).unwrap_err().contains("file_exists::"));
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|m| *m == "new").count(), 1);

    // An open made again with another `run_notebook` gets the open under way, and says how it was made.
    let other = format!("{dir}{SEP}nb0.jl");
    let (release, held) = mpsc::channel();
    *s.engine.hold_open.lock().unwrap() = Some(held);
    let open = |run: bool, began| s.notebooks.tool("a2", "open_notebook", &json!({ "path": other, "run_notebook": run }), &Folder::Process, began).map(tool_json);
    assert!(open(false, late).unwrap_err().starts_with("notebook_opening::Opening"));
    release.send(()).unwrap();
    let opened = open(true, Instant::now()).unwrap();
    assert_eq!((&opened["ran"], &opened["execution_allowed"], opened.get("warnings")), (&json!(false), &json!(false), None), "{opened}");
}

#[test]
fn leaving_safe_preview_that_outlasts_the_call_goes_on_and_the_same_call_again_gets_its_result() {
    let s = setup();
    let dir = temp_notebooks("slow-allow", 1)[0].rsplit_once(SEP).unwrap().0.to_owned();
    let id = s.call("", "open_notebook", json!({ "path": format!("{dir}{SEP}nb0.jl") })).unwrap()["notebook_id"].clone();
    let (release, held) = mpsc::channel();
    *s.engine.hold_open.lock().unwrap() = Some(held);
    let late = Instant::now() - Duration::from_secs(60);
    let allow = |began| s.notebooks.tool("a", "allow_execution", &json!({ "notebook_id": id }), &Folder::Process, began).map(tool_json);
    let started = Instant::now();
    let said = allow(late).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(said.starts_with("notebook_opening::Starting the notebook is taking longer than one call waits"), "{said}");
    assert_eq!(crate::mcp::tool_error(&said, false)["isError"], false, "still starting is not a failure");
    // The same call again while it starts, when the engine reads nothing: it waits for the start, not the read.
    let started = Instant::now();
    assert!(allow(late).unwrap_err().starts_with("notebook_opening::"));
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());

    // It went on; the same call again gets its result, not "already allowed".
    release.send(()).unwrap();
    let allowed = allow(Instant::now()).unwrap();
    assert_eq!((&allowed["already_allowed"], &allowed["ran"], &allowed["execution_allowed"]), (&json!(false), &json!(true), &json!(true)), "{allowed}");
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|m| *m == "allow_execution").count(), 1, "allowed once");
    // Its result was collected and the notebook is still starting: already allowed, not "not in safe preview".
    let again = allow(Instant::now()).unwrap();
    assert_eq!((&again["already_allowed"], &again["ran"], &again["process_status"]), (&json!(true), &json!(false), &json!("starting")), "{again}");
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|m| *m == "allow_execution").count(), 1, "the engine isn't asked again");
}

#[test]
fn opening_and_making_notebooks() {
    let s = setup();
    let dir = temp_notebooks("open", 2)[0].rsplit_once(SEP).unwrap().0.to_owned();
    let path = format!("{dir}{SEP}nb0.jl");
    let opened = s.call("", "open_notebook", json!({ "path": path, "run_notebook": true })).unwrap();
    assert_eq!(opened["warnings"], json!(["async_execution::open queued non-blocking notebook run; poll read_cell for completion"]));
    assert_eq!((&opened["execution_allowed"], &opened["ran"], &opened["process_status"], opened.get("note")), (&json!(true), &json!(true), &json!("starting"), None));
    let previewed = s.call("", "open_notebook", json!({ "path": format!("{dir}{SEP}nb1.jl") })).unwrap();
    assert_eq!((&previewed["execution_allowed"], previewed.get("warnings"), &previewed["note"]), (&json!(false), None, &json!(super::tools::NOT_RUN)));
    assert_eq!(previewed.get("on_cell_change"), None);
    // A notebook in Ember's lazy mode says so (#196).
    let lazy = format!("{dir}{SEP}lazy.jl");
    std::fs::copy(format!("{dir}{SEP}nb1.jl"), &lazy).unwrap();
    assert_eq!(s.call("", "open_notebook", json!({ "path": lazy })).unwrap()["on_cell_change"], "lazy");
    // A script, an empty file or a file of another kind isn't sent to the runtime, which would leave a backup copy of it.
    let opens = || s.engine.calls.lock().unwrap().iter().filter(|call| *call == "open").count();
    let before = opens();
    for (name, text) in [("fit.jl", "using Statistics\nmean([1, 2])\n"), ("fit.R", "x <- mean(c(1, 2))\n"), ("empty.jl", ""), ("notes.txt", "### A Pluto.jl notebook ###\n")] {
        let script = format!("{dir}{SEP}{name}");
        std::fs::write(&script, text).unwrap();
        assert_eq!(s.call("", "open_notebook", json!({ "path": script })), Err(format!("ArgumentError: not_a_notebook::'{script}' isn't a notebook: it doesn't start with a Pluto or Ember notebook's first line")));
    }
    assert_eq!(opens(), before, "the runtime wasn't asked to open them");
    assert_eq!(s.call("", "open_notebook", json!({ "path": format!("{dir}{SEP}none.jl") })), Err(format!("ArgumentError: file_not_found::No file at '{dir}{SEP}none.jl'")));
    assert_eq!(s.call("", "open_notebook", json!({})), Err("ArgumentError: invalid_path::path is required".into()));
    assert_eq!(s.call("", "open_notebook", json!({ "path": path, "run_notebook": "yes" })), Err("TypeError: non-boolean (String) used in boolean context".into()));
    let ember = format!("{dir}{SEP}growth.R");
    std::fs::write(&ember, "### An Ember notebook ###\n# /// environment\n# ///\n").unwrap();
    let opened = s.call("", "open_notebook", json!({ "path": ember }));
    if cfg!(windows) {
        assert_eq!(opened, Err(format!("ArgumentError: unsupported::'{ember}' is an Ember notebook (R). R notebooks don't run on Windows yet; they need macOS or Linux")));
    } else {
        // On macOS and Linux it goes to the R engine, which this test's runtime hasn't.
        assert_eq!(opened, Err("unsupported::R notebooks aren't running in this runtime".into()));
    }

    let made = s.call("", "new_notebook", json!({ "path": format!("{dir}{SEP}.{SEP}fresh.jl") })).unwrap();
    assert_eq!((&made["path"], &made["created"], &made["ran"]), (&json!(format!("{dir}{SEP}fresh.jl")), &json!(true), &json!(true)));
    // Its empty first cell can be edited straight away, without a read first.
    s.edit("", made["notebook_id"].as_str().unwrap(), made["cell_ids"][0].as_str().unwrap(), "x = 1");
    assert_eq!(s.call("", "new_notebook", json!({ "path": path })), Err(format!("ArgumentError: file_exists::'{path}' already exists; use open_notebook to load it")));
    assert_eq!(s.refused("", "new_notebook", json!({ "path": format!("{dir}{SEP}x.txt") })), "invalid_path");
    assert_eq!(s.call("", "new_notebook", json!({ "path": format!("{dir}{SEP}missing{SEP}y.jl") })), Err(format!("ArgumentError: invalid_path::Directory does not exist: '{dir}{SEP}missing'")));
    // A session's folder takes its unnamed notebooks, and relative paths.
    let named = s.notebooks.tool("s", "new_notebook", &json!({ "path": "named.jl" }), &Folder::In(dir.clone()), Instant::now()).map(tool_json).unwrap();
    assert_eq!(named["path"], format!("{dir}{SEP}named.jl"));
    let unnamed = s.notebooks.tool("t", "new_notebook", &json!({}), &Folder::In(dir.clone()), Instant::now()).map(tool_json).unwrap();
    assert_eq!(unnamed["path"], format!("{dir}{SEP}made.jl"));
}

#[test]
fn an_edit_waiting_for_its_run_holds_up_no_other_edit() {
    let s = setup();
    let (a, b) = (id(1), id(2));
    s.engine.open(&a, "/n/a.jl", &[(X, "x = 1"), (Y, "y = 1")]);
    s.engine.open(&b, "/n/b.jl", &[(X, "x = 1")]);
    for (nb, cell) in [(&a, X), (&a, Y), (&b, X)] {
        s.read("7", nb, cell);
        s.read("8", nb, cell);
    }
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *s.engine.hold.lock().unwrap() = Some((started_tx, release_rx));
    let (s, a, b) = (&s, &a, &b);
    std::thread::scope(|scope| {
        let held = scope.spawn(|| s.call("7", "edit_cell", json!({ "notebook_id": a, "cell_id": X, "code": "x = 2", "run_after": true })));
        started.recv_timeout(Duration::from_secs(5)).expect("the held call started");
        let (done_tx, done) = mpsc::channel();
        scope.spawn(move || {
            let other_notebook = s.call("8", "edit_cell", json!({ "notebook_id": b, "cell_id": X, "code": "x = 3" }));
            let other_cell = s.call("7", "edit_cell", json!({ "notebook_id": a, "cell_id": Y, "code": "y = 2" }));
            let events = s.notebooks.subscribe().map(|_| ());
            done_tx.send((other_notebook, other_cell, events)).unwrap();
        });
        let while_held = done.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        let (other_notebook, other_cell, events) = while_held.expect("edits while another waits");
        assert!(other_notebook.is_ok() && other_cell.is_ok() && events.is_ok());
        assert!(held.join().unwrap().is_ok());
    });
    let (first, _) = s.notebooks.subscribe().unwrap();
    let first: Value = serde_json::from_str(&first).unwrap();
    // The edit that ran isn't unrun, so it shows no before-text.
    for (notebook, edited, before) in [(a, X, Value::Null), (a, Y, json!("y = 1")), (b, X, json!("x = 1"))] {
        let state = cell(&first, notebook, edited);
        assert_eq!((&state["author"], &state["before"]), (&json!("agent"), &before), "{notebook} {edited}");
    }
}

#[test]
fn a_failed_edit_is_nobodys() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(Y, "y = 1")]);
    assert_eq!(s.refused("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": Y, "code": "y = 2" })), "read_required");
    s.engine.with(NB, |nb| nb.cells[0].code = "y = 3".into());
    let (first, _) = s.notebooks.subscribe().unwrap();
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(cell(&first, NB, Y)["author"], Value::Null);
}

#[test]
fn arguments_are_refused_as_julia_refused_them() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1")]);
    let read = |args: Value| s.call("", "read_cell", args);
    let not_found = |what: &str, shown: &str, where_: &str| Err(format!("KeyError: key \"{what}_not_found::No {what} with id '{shown}' in {where_}\" not found"));
    assert_eq!(read(json!({ "notebook_id": "nope", "cell_id": X })), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: 'nope'".into()));
    assert_eq!(read(json!({ "notebook_id": Value::Null, "cell_id": X })), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: 'nothing'".into()));
    assert_eq!(
        read(json!({ "notebook_id": 123, "cell_id": X })),
        Err("KeyError: key \"notebook_not_found::No notebook with id '123' in the current session. Run list_notebooks to see what's open.\" not found".into())
    );
    assert_eq!(read(json!({ "notebook_id": NB, "cell_id": "zzz" })), Err("ArgumentError: invalid_cell_id::Invalid cell ID: 'zzz'".into()));
    assert_eq!(read(json!({ "notebook_id": NB, "cell_id": 7 })), not_found("cell", "7", "notebook"));
    assert_eq!(read(json!({ "notebook_id": NB })), Err("KeyError: key \"cell_id\" not found".into()));
    let cell = read(json!({ "notebook_id": NB.to_uppercase(), "cell_id": X })).unwrap();
    assert_eq!(cell, json!({ "cell_id": X, "code": "x = 1", "output": "", "errored": false, "running": false, "queued": false, "code_folded": false, "stale": false }));
    assert_eq!(s.call("", "no_such_tool", json!({})), Err("ArgumentError: unknown_tool::Unknown tool: 'no_such_tool'".into()));
    // A string of cell ids is a string of characters to Julia.
    assert_eq!(
        s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": "1" })),
        Err("ArgumentError: not_staged::Cell 00000000-0000-0000-0000-000000000031 is not in pending_run; stage first or pass force=true".into())
    );
    assert!(s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [X], "force": "yes" })).unwrap_err().starts_with("MethodError: no method matching !(::String)"));
}

fn temp_notebooks(name: &str, count: usize) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("endeavor-notebooks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = wire::files::real_path(&dir).unwrap();
    (0..count)
        .map(|i| {
            let path = dir.join(format!("nb{i}.jl"));
            std::fs::write(&path, "### A Pluto.jl notebook ###\n").unwrap();
            path.display().to_string()
        })
        .collect()
}

fn id(i: usize) -> String {
    format!("aaaaaaaa-0000-0000-0000-00000000000{i}")
}

#[test]
fn one_notebook_per_session() {
    let s = setup();
    let paths = temp_notebooks("one", 4);
    let (first_nb, second_nb) = (&paths[0], &paths[1]);
    let refused = |result: Result<Value, String>| result.err().and_then(|e| e.split_once("::").map(|(kind, _)| kind.to_owned()));
    let one = Some("ArgumentError: one_notebook".to_owned());

    // The first notebook an agent session opens becomes its notebook.
    let first_id = s.call("a", "open_notebook", json!({ "path": first_nb })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(first_nb));
    let refusal = s.call("a", "open_notebook", json!({ "path": second_nb })).unwrap_err();
    assert_eq!(
        refusal,
        format!(
            "ArgumentError: one_notebook::This session works on one notebook, {first_nb}, so it can't open {second_nb}. \
             You can still read other notebooks as plain files. To work on another notebook, suggest the user start a new session with it."
        )
    );
    assert!(s.call("a", "new_notebook", json!({})).unwrap_err().contains("so it can't create another notebook."));
    assert!(s.call("a", "new_notebook", json!({ "path": "/elsewhere/other.jl" })).unwrap_err().contains("so it can't create /elsewhere/other.jl."));
    assert!(s.call("a", "open_notebook", json!({})).unwrap_err().contains("so it can't create another notebook."), "as Julia said it");
    // Its own notebook isn't refused, however it's written.
    let roundabout = format!("{first_nb}{SEP}..{SEP}{}", first_nb.rsplit(SEP).next().unwrap());
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": roundabout }), &Folder::Process), None);
    if let Some(relative) = pathdiff(first_nb, &wire::files::real_path(&std::env::current_dir().unwrap()).unwrap().display().to_string()) {
        assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": relative }), &Folder::Process), None);
    }
    // Julia's expanduser leaves paths alone on Windows.
    #[cfg(unix)]
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": "~bob/x.jl" }), &Folder::Process), Some("ArgumentError: ~user tilde expansion not yet implemented".into()));

    // A relative path starts in the session's folder, not the process's.
    let (folder, name) = first_nb.rsplit_once(SEP).unwrap();
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": name }), &Folder::In(folder.to_owned())), None);
    let other = second_nb.rsplit_once(SEP).unwrap().1;
    assert!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": other }), &Folder::In(folder.to_owned())).unwrap().contains("one_notebook"));

    // Calls without an owner (the app, tests) are unrestricted.
    let second_id = s.call("", "open_notebook", json!({ "path": second_nb })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    s.notebooks.publish();

    // Writes and runs on another open notebook are refused; reads are not.
    assert_eq!(refused(s.call("a", "edit_cell", json!({ "notebook_id": second_id, "cell_id": X, "code": "1" }))), one);
    assert_eq!(
        s.call("a", "run_all_cells", json!({ "notebook_id": second_id })).unwrap_err(),
        format!(
            "ArgumentError: one_notebook::This session works on one notebook, {first_nb}, so it can't change or run {second_nb}. \
             You can still read other notebooks as plain files. To work on another notebook, suggest the user start a new session with it."
        )
    );
    assert_eq!(refused(s.call("a", "read_notebook_code", json!({ "notebook_id": second_id }))), None);
    assert_eq!(refused(s.call("a", "run_all_cells", json!({ "notebook_id": first_id }))), None);
    assert_eq!(refused(s.call("a", "run_all_cells", json!({ "notebook_id": "not-a-uuid" }))), Some("ArgumentError: invalid_notebook_id".into()), "the tool says what's wrong");

    // A binding set by the app is respected, and clearing it lifts the limit.
    s.notebooks.bind("b", second_nb);
    assert_eq!(refused(s.call("b", "open_notebook", json!({ "path": first_nb }))), one);
    assert_eq!(refused(s.call("b", "run_all_cells", json!({ "notebook_id": first_id }))), one);
    assert_eq!(refused(s.call("b", "get_cell_order", json!({ "notebook_id": first_id }))), None);
    s.notebooks.bind("b", "");
    assert_eq!(s.notebooks.bound("b"), None);
    assert_eq!(refused(s.call("b", "run_all_cells", json!({ "notebook_id": first_id }))), None);

    // A "New notebook" session: the notebook it creates becomes its notebook.
    std::fs::remove_file(&paths[2]).unwrap();
    s.call("c", "new_notebook", json!({ "path": &paths[2] })).unwrap();
    assert_eq!(s.notebooks.bound("c").as_ref(), Some(&paths[2]));
    assert_eq!(refused(s.call("c", "new_notebook", json!({ "path": &paths[3] }))), one);
    assert_eq!(refused(s.call("c", "open_notebook", json!({ "path": first_nb }))), one);
}

#[test]
fn opening_an_open_notebook_joins_it_and_changes_nothing() {
    let s = setup();
    let paths = temp_notebooks("join", 2);
    let first = s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    assert_eq!(first.get("already_open"), None);
    let id = first["notebook_id"].as_str().unwrap().to_owned();
    s.engine.with(&id, |nb| nb.safe_preview = true);
    let calls = s.engine.calls.lock().unwrap().len();

    // Another session, and a call without a session (the app's), get the open notebook as it is.
    let joined = s.call("b", "open_notebook", json!({ "path": &paths[0], "run_notebook": true })).unwrap();
    assert_eq!(
        joined,
        json!({ "notebook_id": id, "path": paths[0], "execution_allowed": false, "ran": false, "process_status": "waiting_for_permission", "already_open": true, "note": super::tools::NOT_RUN })
    );
    assert_eq!(s.notebooks.bound("b").as_ref(), Some(&paths[0]));
    let apps = s.call("", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    assert_eq!((&apps["notebook_id"], &apps["already_open"], s.notebooks.bound("")), (&json!(id), &json!(true), None));
    assert!(s.engine.with(&id, |nb| nb.safe_preview), "still in safe preview");
    let made = s.engine.calls.lock().unwrap()[calls..].iter().filter(|call| matches!(call.as_str(), "run" | "apply" | "allow")).count();
    assert_eq!(made, 0, "nothing ran or changed");

    // A session bound to another notebook is still refused; its own notebook joins as always.
    s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    assert!(s.call("c", "open_notebook", json!({ "path": &paths[0] })).unwrap_err().starts_with("ArgumentError: one_notebook::"));
    assert_eq!(s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap()["already_open"], true);
    assert_eq!(s.notebooks.bound("c").as_ref(), Some(&paths[1]));

    // Other adapter errors stay errors.
    assert!(!super::tools::already_open("ArgumentError: file_not_found::'x' is already open as notebook_id y; use that id"));
    assert!(super::tools::already_open("ArgumentError: notebook_already_open::any wording"));
}

#[test]
fn joining_an_open_notebook_reads_that_notebook_and_no_other() {
    let s = setup();
    let paths = temp_notebooks("join-one", 2);
    let first = s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    let second = s.call("b", "open_notebook", json!({ "path": &paths[1] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    s.engine.with(&second, |nb| nb.broken = true);
    assert!(s.notebooks.snapshots().is_err(), "a snapshot of every notebook fails");
    s.engine.calls.lock().unwrap().clear();
    let joined = s.call("c", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    assert_eq!((&joined["notebook_id"], &joined["already_open"]), (&json!(first), &json!(true)));
    assert_eq!(s.notebooks.bound("c").as_ref(), Some(&paths[0]));
    let calls = s.engine.calls.lock().unwrap().clone();
    assert!(calls.contains(&"status".to_owned()), "found by its path in the list of open notebooks: {calls:?}");
}

#[test]
fn a_session_bound_again_after_its_record_went_stale_is_not_dropped() {
    let s = setup();
    let paths = temp_notebooks("rebound", 2);
    s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    s.seconds(8.0 * 24.0 * 3600.0);
    // `a`'s record is a week old and not yet cleared when the app binds it, or it opens its notebook, again.
    s.notebooks.bind("a", &paths[0]);
    s.notebooks.note_call("d");
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[0]));
    assert!(s.notebooks.state.lock().unwrap().seen.contains_key("a"));
    // `c` was not, so its record and then its binding went with it.
    assert_eq!(s.notebooks.bound("c"), None);
    assert!(!s.notebooks.state.lock().unwrap().bindings.contains_key("c"));

    s.seconds(8.0 * 24.0 * 3600.0);
    s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.notebooks.note_call("e");
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[0]), "opening an open notebook again keeps the session too");
}

#[test]
fn recent_sessions_are_the_other_sessions_that_called_within_the_time_given() {
    let s = setup();
    let paths = temp_notebooks("others", 2);
    s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    let recent = |owner: &str, within: f64| s.notebooks.recent_sessions(owner, within).unwrap();
    assert_eq!(recent("a", 900.0), json!({ "count": 0, "active_seconds_ago": null }), "a session with no call is not active");

    s.call("b", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.notebooks.note_call("b");
    s.seconds(125.7);
    assert_eq!(recent("a", 900.0), json!({ "count": 1, "active_seconds_ago": 125 }));
    assert_eq!(recent("b", 900.0)["count"], 0, "a session is not another to itself");

    s.notebooks.note_call("c");
    s.seconds(10.0);
    assert_eq!(recent("a", 900.0), json!({ "count": 2, "active_seconds_ago": 10 }));
    assert_eq!(recent("a", 60.0), json!({ "count": 1, "active_seconds_ago": 10 }), "b called longer ago than that");
    assert_eq!(recent("", 900.0)["count"], 2, "the app's calls have no session, so every session is another");

    // A session the app bound has made no call; one that is bound to no notebook doesn't count however lately it called;
    // nor does one whose notebook has closed.
    s.notebooks.bind("app", &paths[0]);
    s.notebooks.note_call("unbound");
    assert_eq!(recent("a", 900.0)["count"], 2);
    s.engine.notebooks.lock().unwrap().retain(|nb| nb.path != paths[1]);
    assert_eq!(recent("a", 900.0), json!({ "count": 1, "active_seconds_ago": 135 }), "c's notebook is closed, and b called 135 s ago");
    assert!(s.notebooks.state.lock().unwrap().seen.contains_key("unbound"));
    s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    s.notebooks.note_call("c");

    // Clearing a binding keeps the record of the last call, which goes a week after it.
    s.notebooks.bind("b", "");
    assert!(s.notebooks.state.lock().unwrap().seen.contains_key("b"));
    assert_eq!(recent("a", 900.0)["count"], 1, "b works in no notebook now");
    s.notebooks.note_call("");
    assert!(!s.notebooks.state.lock().unwrap().seen.contains_key(""), "a call with no session isn't recorded");
    assert_eq!(s.notebooks.state.lock().unwrap().seen.len(), 5, "a, b, c, unbound and the app's bound session");

    // A week after its last call, or after it was bound if it made none, a session is forgotten.
    s.seconds(6.0 * 24.0 * 3600.0);
    s.notebooks.note_call("a");
    s.seconds(2.0 * 24.0 * 3600.0);
    assert_eq!(recent("c", 1.0e9), json!({ "count": 1, "active_seconds_ago": 172800 }));
    assert_eq!(s.notebooks.state.lock().unwrap().seen.len(), 1, "c and the app's session went 8 days ago, a 2");
}

#[test]
fn a_binding_to_a_notebook_that_is_no_longer_open_is_no_binding_and_one_that_is_open_stays() {
    let s = setup();
    let paths = temp_notebooks("returned", 3);
    s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.call("b", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    let close = |path: &str| s.engine.notebooks.lock().unwrap().retain(|nb| nb.path != path);
    let mine = |owner: &str| s.call(owner, "list_notebooks", json!({})).unwrap().as_array().unwrap().iter().filter(|nb| nb["this_session"] == true).count();
    assert_eq!((mine("a"), mine("b")), (1, 1));

    // `a`'s notebook is closed while it is away: it can make another, and open one.
    close(&paths[0]);
    assert!(s.call("a", "open_notebook", json!({ "path": &paths[2] })).is_ok());
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[2]));
    // The same, found by `list_notebooks` instead of by the refusal.
    close(&paths[2]);
    assert_eq!(mine("a"), 0);
    assert_eq!(s.notebooks.bound("a"), None);
    assert!(s.call("a", "new_notebook", json!({})).is_ok());

    // `b`'s is still open: it is still in it, and is held to it.
    assert_eq!(mine("b"), 1);
    let refused = s.call("b", "open_notebook", json!({ "path": &paths[0] })).unwrap_err();
    assert!(refused.contains("one_notebook::") && refused.contains(&paths[1]), "{refused}");
    assert_eq!(s.notebooks.bound("b").as_ref(), Some(&paths[1]));
}

#[test]
fn list_notebooks_says_which_notebook_is_this_sessions() {
    let s = setup();
    let paths = temp_notebooks("mine", 2);
    let first = s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    let second = s.call("", "open_notebook", json!({ "path": &paths[1] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    let mine = |owner: &str| -> HashMap<String, Value> {
        let listed = s.call(owner, "list_notebooks", json!({})).unwrap();
        listed.as_array().unwrap().iter().map(|nb| (nb["notebook_id"].as_str().unwrap().to_owned(), nb["this_session"].clone())).collect()
    };
    assert_eq!(mine("a"), HashMap::from([(first.clone(), json!(true)), (second.clone(), json!(false))]));
    assert_eq!(mine("unbound"), HashMap::from([(first.clone(), json!(false)), (second.clone(), json!(false))]));
    assert_eq!(mine(""), HashMap::from([(first, json!(false)), (second, json!(false))]), "the app has no notebook of its own");
}

#[test]
fn the_apps_notebook_actions_restart_move_file_info_and_new_notebook() {
    let s = setup();
    let paths = temp_notebooks("actions", 2);
    let dir = paths[0].rsplit_once(SEP).unwrap().0.to_owned();
    s.engine.open(&id(1), &paths[0], &[(X, "x = 1"), (Y, "y = x")]);
    s.engine.open(&id(2), &paths[1], &[]);
    s.engine.with(&id(2), |nb| nb.safe_preview = true);

    // Restart runs every cell, so edits waiting for a run stop waiting; it's refused in safe preview.
    s.read("", &id(1), Y);
    s.edit("", &id(1), Y, "y = x + 1");
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([Y]));
    s.seconds(1.0);
    assert_eq!(s.notebooks.restart(&id(1)), Ok(json!({ "restarted": true })));
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([]));
    assert_eq!(s.notebooks.restart(&id(2)), Err("ArgumentError: execution_blocked::The notebook is in safe preview; Run notebook starts it".into()));
    assert_eq!(s.notebooks.restart("nope"), Err("KeyError: key \"notebook_not_found::No notebook with id 'nope' in the current session\" not found".into()));

    // A move takes the sessions bound to the notebook, and an idle-stopped entry for it, along.
    s.notebooks.bind("a", &paths[0]);
    s.notebooks.state.lock().unwrap().idle_stopped.push((paths[0].clone(), json!({ "path": &paths[0], "hours": 1, "safe_preview": false })));
    let renamed = format!("{dir}{SEP}renamed.jl");
    assert_eq!(s.notebooks.move_notebook(&id(1), &renamed), Ok(json!({ "path": renamed })));
    assert_eq!(s.notebooks.bound("a"), Some(renamed.clone()));
    assert_eq!(s.notebooks.idle_stopped(), [json!({ "path": renamed, "hours": 1, "safe_preview": false })]);
    assert_eq!(s.notebooks.refusal("a", "edit_cell", &json!({ "notebook_id": id(1) }), &Folder::Process), None, "still its own notebook");
    assert_eq!(s.notebooks.move_notebook(&id(1), &paths[1]), Err(format!("ArgumentError: file_exists::'{}' already exists", paths[1])));
    assert_eq!(s.notebooks.move_notebook(&id(1), &format!("{dir}{SEP}notes.txt")), Err(format!("ArgumentError: invalid_path::Notebook path must end in .jl: '{dir}{SEP}notes.txt'")));
    assert_eq!(s.notebooks.move_notebook(&id(1), &format!("{dir}{SEP}gone{SEP}x.jl")), Err(format!("ArgumentError: invalid_path::Directory does not exist: '{dir}{SEP}gone'")));

    // Whether a file is there, and when it last changed, as Julia's mtime says.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&paths[1]).unwrap();
        let modified = meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9;
        assert_eq!(file_info(&paths[1]), Ok(json!({ "exists": true, "modified": modified })));
    }
    assert_eq!(file_info(&paths[0]), Ok(json!({ "exists": false })));
    assert_eq!(file_info(&dir), Ok(json!({ "exists": false })), "a folder isn't a file");

    // The app's New notebook for a session: in its folder, and the session's notebook from now on.
    s.notebooks.bind("b", &renamed);
    let made = s.notebooks.new_for("b", &Folder::In(dir.clone())).unwrap();
    assert_eq!(made["path"], format!("{dir}{SEP}made.jl"));
    assert_eq!(s.notebooks.bound("b"), Some(format!("{dir}{SEP}made.jl")));
}

/// `path` relative to `from`, through `..`s; none on Windows across drives.
fn pathdiff(path: &str, from: &str) -> Option<String> {
    let (path, from): (Vec<_>, Vec<_>) = (path.split(SEP).filter(|p| !p.is_empty()).collect(), from.split(SEP).filter(|p| !p.is_empty()).collect());
    let common = path.iter().zip(&from).take_while(|(a, b)| a == b).count();
    if cfg!(windows) && common == 0 {
        return None;
    }
    let mut parts = vec![".."; from.len() - common];
    parts.extend(&path[common..]);
    Some(parts.join(std::path::MAIN_SEPARATOR_STR))
}

#[test]
fn stop_notebook_shuts_it_down_and_says_if_it_was_in_safe_preview() {
    let s = setup();
    let paths = temp_notebooks("stop", 2);
    s.engine.open(&id(1), &paths[0], &[]);
    s.engine.with(&id(1), |nb| nb.safe_preview = true);
    s.engine.open(&id(2), &format!("{}{SEP}..{SEP}{}", paths[1].rsplit_once(SEP).unwrap().0, "nb1.jl"), &[]);
    assert_eq!(s.notebooks.stop_notebook(&paths[0]), Ok(json!({ "stopped": true, "safe_preview": true })));
    assert_eq!(s.notebooks.stop_notebook(&paths[0]), Ok(json!({ "stopped": false })));
    assert_eq!(s.notebooks.stop_notebook(&paths[1]), Ok(json!({ "stopped": false })), "a path that only looks like it");
    s.engine.with(&id(2), |nb| nb.path = paths[1].clone());
    assert_eq!(s.notebooks.stop_notebook(&paths[1]), Ok(json!({ "stopped": true, "safe_preview": false })));
    assert_eq!(*s.engine.shut_down.lock().unwrap(), [id(1), id(2)]);
}

#[test]
fn idle_notebooks_stop_but_running_kept_alive_and_recently_used_ones_dont() {
    let s = setup();
    let paths = temp_notebooks("idle", 4);
    let (idle, running, kept, used) = (&paths[0], &paths[1], &paths[2], &paths[3]);
    for (i, path) in paths.iter().enumerate() {
        s.engine.open(&id(i), path, &[(X, "x = 1")]);
        s.notebooks.notified(&json!({ "method": "notebook_opened", "params": { "notebook_id": id(i), "path": path } }));
    }
    s.engine.with(&id(0), |nb| nb.safe_preview = true);
    assert_eq!(s.call("", "keep_notebook_alive", json!({ "notebook_id": id(2), "keep": true })), Ok(json!({ "notebook_id": id(2), "kept_alive": true })));
    s.engine.with(&id(1), |nb| nb.cells[0].running = true);
    let open = || s.engine.notebooks.lock().unwrap().iter().map(|nb| nb.path.clone()).collect::<Vec<_>>();
    let (_, rx) = s.notebooks.subscribe().unwrap();

    s.hours(47.0);
    assert!(s.notebooks.stop_idle().is_empty());
    s.call("", "read_notebook_code", json!({ "notebook_id": id(3) })).unwrap();
    s.hours(2.0);
    assert_eq!(s.notebooks.stop_idle(), [idle.clone()]);
    assert_eq!(open(), [running.clone(), kept.clone(), used.clone()]);
    let stopped = json!([{ "path": idle, "hours": 48, "safe_preview": true }]);
    assert_eq!(s.notebooks.idle_stopped(), stopped.as_array().unwrap().clone());
    assert_eq!(next(&rx)["idle_stopped"], stopped, "the app hears why");

    // A run's clock starts when it's last seen running; the tool call's at the call.
    s.engine.with(&id(1), |nb| nb.cells[0].running = false);
    s.hours(46.0);
    assert_eq!(s.notebooks.stop_idle(), [used.clone()]);
    s.hours(2.0);
    assert_eq!(s.notebooks.stop_idle(), [running.clone()]);
    assert_eq!(open(), [kept.clone()]);

    // Kept alive until turned off; then the usual limit applies from then.
    s.hours(500.0);
    assert!(s.notebooks.stop_idle().is_empty());
    s.call("", "keep_notebook_alive", json!({ "notebook_id": id(2), "keep": false })).unwrap();
    s.hours(47.0);
    assert!(s.notebooks.stop_idle().is_empty());
    s.hours(1.0);
    assert_eq!(s.notebooks.stop_idle(), [kept.clone()]);
    assert!(open().is_empty());
    assert!(s.notebooks.state.lock().unwrap().notebooks.is_empty(), "their state went with them");
    assert_eq!(s.notebooks.idle_stopped().len(), 4, "kept so the app can say why each stopped");

    // Opening a stopped notebook again clears its record; 0 means never stop.
    s.engine.open(&id(0), idle, &[]);
    s.notebooks.notified(&json!({ "method": "notebook_opened", "params": { "notebook_id": id(0), "path": idle } }));
    assert!(s.notebooks.idle_stopped().iter().all(|e| e["path"] != idle.as_str()));
    s.notebooks.set_idle_limit(0.0);
    s.hours(10_000.0);
    assert!(s.notebooks.stop_idle().is_empty());
    s.notebooks.set_idle_limit(1.5);
    s.hours(2.0);
    assert_eq!(s.notebooks.stop_idle(), [idle.clone()]);
    assert_eq!(s.notebooks.idle_stopped().last().unwrap()["hours"], 2, "rounded as Julia rounds");
    // Saves and finished runs count as activity.
    s.engine.open(&id(5), used, &[]);
    s.notebooks.notified(&json!({ "method": "notebook_opened", "params": { "notebook_id": id(5), "path": used } }));
    s.hours(1.0);
    s.notebooks.notified(&json!({ "method": "file_saved", "params": { "notebook_id": id(5) } }));
    s.hours(1.0);
    assert!(s.notebooks.stop_idle().is_empty());
    s.notebooks.notified(&json!({ "method": "execution_done", "params": { "notebook_id": id(5) } }));
    s.hours(1.4);
    assert!(s.notebooks.stop_idle().is_empty());
}

#[test]
fn a_notebooks_state_goes_when_it_shuts_down_however_it_shuts_down() {
    let s = setup();
    let paths = temp_notebooks("gone", 3);
    s.engine.open(NB, &paths[0], &[(X, "x = 1")]);
    s.read("7", NB, X);
    s.edit("7", NB, X, "x = 2");
    s.call("", "keep_notebook_alive", json!({ "notebook_id": NB, "keep": true })).unwrap();
    let held = || {
        s.notebooks.state.lock().unwrap().notebooks.get(NB).map(|nb| {
            (nb.authors.len(), nb.befores.len(), nb.kept_alive, nb.last_active.is_some(), nb.pending.len(), nb.reads.len(), nb.changes.len())
        })
    };
    assert_eq!(held(), Some((1, 1, true, true, 1, 1, 1)));

    // Restarting in place (leaving safe preview) keeps it in the session, and its state.
    s.notebooks.notified(&json!({ "method": "cell_state", "params": { "notebook_id": NB, "cells": [] } }));
    s.notebooks.publish();
    assert_eq!(held(), Some((1, 1, true, true, 1, 1, 1)));

    // Pluto shutting it down (the page's own button).
    s.engine.notebooks.lock().unwrap().clear();
    s.notebooks.notified(&json!({ "method": "notebook_shut_down", "params": { "notebook_id": NB } }));
    assert_eq!(held(), None);

    // Stopped by the app, or gone by the time the core looks.
    s.engine.open(NB, &paths[1], &[(X, "x = 1")]);
    s.call("", "keep_notebook_alive", json!({ "notebook_id": NB, "keep": true })).unwrap();
    s.notebooks.stop_notebook(&paths[1]).unwrap();
    assert_eq!(held(), None);
    s.engine.open(NB, &paths[2], &[(X, "x = 1")]);
    s.call("", "keep_notebook_alive", json!({ "notebook_id": NB, "keep": true })).unwrap();
    s.engine.notebooks.lock().unwrap().clear();
    s.notebooks.publish();
    assert_eq!(held(), None);
}

#[test]
fn keep_notebook_alive_checks_its_arguments_as_julia_did() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[]);
    let keep = |nid: Value, keep: Value| s.notebooks.keep_alive(&json!({ "notebook_id": nid, "keep": keep }));
    let missing = "cccccccc-0000-0000-0000-000000000000";
    assert_eq!(keep(json!("nope"), json!(true)), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: 'nope'".into()));
    assert_eq!(keep(Value::Null, json!(true)), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: 'nothing'".into()));
    assert_eq!(keep(json!(-1), json!(true)), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: '-1'".into()));
    let not_found =
        |shown: &str| Err(format!("KeyError: key \"notebook_not_found::No notebook with id '{shown}' in the current session. Run list_notebooks to see what's open.\" not found"));
    assert_eq!(keep(json!(missing), json!(true)), not_found(missing));
    assert_eq!(keep(json!(123), json!(true)), not_found("123"));
    assert_eq!(s.notebooks.keep_alive(&json!({ "keep": true })), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: ''".into()));
    assert_eq!(keep(json!(NB), json!("yes")), Err("ArgumentError: invalid_keep::keep must be true or false".into()));
    assert_eq!(keep(json!(NB.to_uppercase()), json!(false)), Ok(json!({ "notebook_id": NB, "kept_alive": false })));
    let text = |raw: &str| crate::mcp::tool_error(raw, false)["content"][0]["text"].as_str().unwrap().to_owned();
    assert_eq!(
        text(&keep(json!(missing), json!(true)).unwrap_err()),
        format!(r#"{{"error":"notebook_not_found","message":"No notebook with id '{missing}' in the current session. Run list_notebooks to see what's open."}}"#)
    );
}

#[test]
fn parses_ids_and_paths_as_julia_did() {
    assert_eq!(parse_uuid("AAAAAAAA-1111-1111-1111-111111111111").as_deref(), Some("aaaaaaaa-1111-1111-1111-111111111111"));
    for bad in ["11111111111111111111111111111111", "{11111111-1111-1111-1111-111111111111}", " 11111111-1111-1111-1111-111111111111", "x", ""] {
        assert_eq!(parse_uuid(bad), None, "{bad}");
    }
    assert_eq!(uuid_of(123), "00000000-0000-0000-0000-00000000007b");
    let dir = temp_notebooks("paths", 1)[0].rsplit_once(SEP).unwrap().0.to_owned();
    assert_eq!(canonical_path(&format!("{dir}{SEP}.{SEP}nb0.jl")).unwrap(), format!("{dir}{SEP}nb0.jl"));
    assert_eq!(canonical_path(&format!("{dir}{SEP}new.jl")).unwrap(), format!("{dir}{SEP}new.jl"), "a file yet to be made");
    assert_eq!(canonical_path(&format!("{dir}/./nb0.jl")).unwrap(), format!("{dir}{SEP}nb0.jl"), "either slash on Windows");
    #[cfg(unix)]
    {
        assert_eq!(canonical_path("/no/such/dir/../x.jl").unwrap(), "/no/such/x.jl");
        assert_eq!(canonical_path("~").unwrap(), std::fs::canonicalize(home()).unwrap().display().to_string());
        // /tmp is a symlink on macOS: paths resolve through it.
        if let Ok(real) = std::fs::canonicalize("/tmp") {
            assert_eq!(canonical_path("/tmp/endeavor-no-such.jl").unwrap(), format!("{}/endeavor-no-such.jl", real.display()));
        }
    }
    #[cfg(windows)]
    {
        assert_eq!(canonical_path(r"Q:\no\such\dir\..\x.jl").unwrap(), r"Q:\no\such\x.jl");
        assert_eq!(canonical_path("Q:/no/such/x.jl").unwrap(), r"Q:\no\such\x.jl");
        assert!(is_absolute(r"C:\x") && is_absolute("C:/x") && is_absolute(r"\x") && !is_absolute(r"x\y") && !is_absolute("C:x"));
    }
}

#[test]
fn a_notebooks_own_julia_ending_by_itself_ends_the_run_claude_waits_for() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 6"), (Y, "rates = crash()")]);
    let (_, rx) = s.notebooks.subscribe().unwrap();

    let error = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true })).expect_err("the run ended");
    let shown: Value = serde_json::from_str(crate::mcp::tool_error(&error, false)["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        shown,
        json!({
            "error": "process_exited",
            "message": "Julia stopped unexpectedly while running `rates`. The notebook file is saved; its outputs are gone until the cells run again."
        })
    );

    // The app hears which cell was running; nothing runs any more.
    assert!(s.notebooks.notified(&json!({ "method": "process_exited", "params": { "notebook_id": NB, "running": [Y] } })));
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!(
        event["notebooks"][0],
        json!({ "notebook_id": NB, "path": "/n/a.jl", "cell_count": 2, "pending_run": [], "running": [], "execution_allowed": false,
                "this_session": false, "exited": { "running": [Y] } })
    );
    assert_eq!(s.call("7", "list_notebooks", json!({})).unwrap()[0]["exited"], json!({ "running": [Y] }));
    assert_eq!(
        s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap()["execution"]["status"],
        "blocked",
        "a run after it is refused as before"
    );

    // Restarted: no longer said.
    s.engine.with(NB, |nb| nb.exited = None);
    s.notebooks.publish();
    assert_eq!(next(&rx)["notebooks"][0].get("exited"), None);

    // A cell that defines nothing is named by its id; a run not waited for just runs.
    s.engine.with(NB, |nb| nb.cells[1].code = "crash()".into());
    let error = s.call("7", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).expect_err("the run ended");
    assert!(error.ends_with(&format!("process_exited::Julia stopped unexpectedly while running `{Y}`. The notebook file is saved; its outputs are gone until the cells run again.")), "{error}");
    s.engine.with(NB, |nb| nb.exited = None);
    assert_eq!(s.call("7", "run_all_cells", json!({ "notebook_id": NB })).unwrap()["execution"]["status"], "running");
}

#[test]
fn a_cell_is_never_named_after_its_anonymous_functions() {
    let graph = Graph::parse(&json!({ "cells": [
        { "cell_id": X, "definitions": ["c"], "functions": ["__ExprExpl_anon__7243518869190234"], "references": [] },
        { "cell_id": Y, "definitions": [], "functions": ["__ExprExpl_anon__11"], "references": ["c"] },
    ] }));
    assert_eq!((graph.name(X), graph.name(Y)), (Some("c".to_owned()), None));
}

#[cfg(unix)]
#[test]
fn a_requested_path_expands_the_home_folder_before_the_session_folder() {
    let work = Folder::In("/work".into());
    assert_eq!(requested_path("~/x.jl", &work), Ok(format!("{}/x.jl", home())));
    assert_eq!(requested_path("~/x.jl", &Folder::Process), Ok(format!("{}/x.jl", home())));
    assert_eq!(requested_path("x.jl", &work), Ok("/work/x.jl".to_owned()));
    assert_eq!(requested_path("/a/x.jl", &work), Ok("/a/x.jl".to_owned()));
}

#[cfg(unix)]
#[test]
fn a_session_with_no_folder_may_give_absolute_paths_and_nothing_else() {
    let none = Folder::Unknown;
    assert_eq!(requested_path("~/x.jl", &none), Ok(format!("{}/x.jl", home())));
    assert_eq!(requested_path("/a/x.jl", &none), Ok("/a/x.jl".to_owned()));
    let refused = tools::argument_error("invalid_path::Give an absolute path: this server was not told the project folder.");
    assert_eq!(requested_path("x.jl", &none), Err(refused.clone()));
    assert_eq!(requested_path("./a/../x.jl", &none), Err(refused.clone()));
    assert_eq!(requested_path("", &none), Err(refused.clone()));
    for (tool, arguments) in [("open_notebook", json!({ "path": "x.jl" })), ("new_notebook", json!({ "path": "x.jl" })), ("new_notebook", json!({})), ("new_notebook", json!({ "path": null }))] {
        assert_eq!(path_refusal(tool, &arguments, &none), Some((refused.clone(), true)), "{tool} {arguments}");
        assert_eq!(path_refusal(tool, &arguments, &Folder::In("/work".into())), None, "a session with a folder");
        assert_eq!(path_refusal(tool, &arguments, &Folder::Process), None, "the app's session");
    }
    assert_eq!(
        path_refusal("open_notebook", &json!({ "path": "~bob/x.jl" }), &none),
        Some(("ArgumentError: invalid_path::~user tilde expansion not yet implemented".to_owned(), false)),
        "the kind of a bad path, and not told to read the guide as the refusal that says what to do is not"
    );
    for (tool, arguments) in [("open_notebook", json!({ "path": "/a/x.jl" })), ("new_notebook", json!({ "path": "/a/x.jl" })), ("open_notebook", json!({})), ("open_notebook", json!({ "path": 5 })), ("read_cell", json!({ "path": "x.jl" })), ("list_notebooks", json!({}))] {
        assert_eq!(path_refusal(tool, &arguments, &none), None, "{tool} {arguments}");
    }
}

#[cfg(windows)]
#[test]
fn a_path_with_no_drive_is_not_absolute_enough_for_a_session_with_no_folder() {
    for path in ["C:\\x.jl", "c:/x.jl", "\\\\server\\share\\x.jl"] {
        assert!(is_fully_absolute(path), "{path}");
    }
    for path in ["\\x.jl", "/x.jl", "x.jl", "C:x.jl"] {
        assert!(!is_fully_absolute(path), "{path}");
        assert!(requested_path(path, &Folder::Unknown).is_err(), "{path}");
    }
}

#[test]
fn a_second_engines_notebooks_are_listed_and_its_calls_go_to_it() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "a = 1")]);
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(Y, "b = 2")]);
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    let listed = s.call("", "list_notebooks", json!({})).unwrap();
    let ids: Vec<&str> = listed.as_array().unwrap().iter().map(|nb| nb["notebook_id"].as_str().unwrap()).collect();
    assert_eq!(ids, [NB, R]);
    s.read("", R, Y);
    s.edit("", R, Y, "b = 3");
    assert_eq!(ember.code(R, Y), "b = 3");
    assert_eq!(s.engine.code(NB, X), "a = 1");
    // A move keeps an R notebook an R notebook.
    let dir = std::env::temp_dir().to_string_lossy().trim_end_matches(SEP).to_owned();
    for name in ["c.jl", "c.txt"] {
        assert_eq!(s.notebooks.move_notebook(R, &format!("{dir}{SEP}{name}")), Err(format!("ArgumentError: invalid_path::Notebook path must end in .R: '{dir}{SEP}{name}'")));
    }
    assert_eq!(s.notebooks.move_notebook(NB, &format!("{dir}{SEP}c.R")), Err(format!("ArgumentError: invalid_path::Notebook path must end in .jl: '{dir}{SEP}c.R'")), "and a Julia one a Julia one");
}

/// A notebook made while the app's summary is being put together isn't in that summary's snapshots,
/// and the reads new_notebook recorded of its cells stay: its first edit needs no read first.
#[test]
fn a_notebook_made_during_a_summary_keeps_its_reads() {
    let s = setup();
    let dir = std::env::temp_dir().to_string_lossy().trim_end_matches(SEP).to_owned();
    let (read_tx, read) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *s.engine.hold_snapshot.lock().unwrap() = Some((read_tx, release_rx));
    std::thread::scope(|scope| {
        let publishing = scope.spawn(|| s.notebooks.publish());
        read.recv().unwrap();
        let making = scope.spawn(|| s.call("", "new_notebook", json!({ "path": format!("{dir}{SEP}endeavor-made-during-a-summary.jl") })).unwrap());
        // new_notebook has recorded its reads, and then waits for the summary.
        std::thread::sleep(Duration::from_millis(300));
        release.send(()).unwrap();
        publishing.join().unwrap();
        let made = making.join().unwrap();
        let (id, cell) = (made["notebook_id"].as_str().unwrap(), made["cell_ids"][0].as_str().unwrap());
        assert!(s.call("", "edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "x = 1" })).is_ok(), "no read needed");
    });
}

#[test]
fn what_the_engine_knows_of_a_cell_shows_in_read_cell() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "a = 1")]);
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "a <- 1"), (Y, "b <- a + 1")]);
    ember.with(R, |nb| {
        nb.cells[0].not_run = true;
        nb.cells[1].stale = true;
    });
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("", "list_notebooks", json!({})).unwrap();
    let read = |nb: &str, cell: &str| s.call("", "read_cell", json!({ "notebook_id": nb, "cell_id": cell })).unwrap();
    let (not_run, stale) = (read(R, X), read(R, Y));
    assert_eq!((&not_run["not_run"], &not_run["stale"]), (&json!(true), &json!(false)));
    assert_eq!((&stale["stale"], stale.get("not_run")), (&json!(true), None), "not_run shows only when it is so");
    assert_eq!(read(NB, X).get("not_run"), None, "and never for Julia");
    let code = s.call("", "read_notebook_code", json!({ "notebook_id": R })).unwrap();
    assert_eq!(code["stale_cell_ids"], json!([Y]), "and in the whole notebook's read: {code}");
}

#[test]
fn an_r_notebooks_package_install_says_r_is_getting_them_ready() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "library(dplyr)"), (Y, "summarise(mtcars, m = mean(mpg))")]);
    ember.with(R, |nb| {
        nb.packages = Some(json!({ "step": "installing", "packages": ["dplyr"], "seconds": 20, "last_line": "installing dplyr" }));
        nb.cells.iter_mut().for_each(|c| c.queued = true);
    });
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("", "list_notebooks", json!({})).unwrap();
    let began = Instant::now().checked_sub(Duration::from_secs(44)).expect("a clock that has run that long");
    let read = super::tools::tool_json(s.notebooks.tool("", "read_cell", &json!({ "notebook_id": R, "cell_id": Y }), &Folder::Process, began).unwrap());
    let message = read["message"].as_str().unwrap();
    assert!(message.starts_with("R is getting dplyr ready for this notebook (installing, 20 seconds so far)."), "{message}");
    assert_eq!(read["packages"]["packages"], json!(["dplyr"]));
}

#[test]
fn new_notebook_takes_an_r_path_only_where_r_notebooks_open() {
    let s = setup();
    let dir = std::env::temp_dir().to_string_lossy().trim_end_matches(SEP).to_owned();
    let made = s.call("", "new_notebook", json!({ "path": format!("{dir}{SEP}endeavor-new-r-test.R") }));
    let other = s.call("", "new_notebook", json!({ "path": format!("{dir}{SEP}notes.txt") })).unwrap_err();
    if cfg!(windows) {
        let refused = made.unwrap_err();
        assert!(refused.contains("unsupported::") && refused.contains("R notebooks don't run on Windows yet"), "{refused}");
        assert!(other.contains("invalid_path::Notebook path must end in .jl: "), "R isn't offered where R notebooks can't open: {other}");
    } else {
        assert_eq!(made, Err("unsupported::R notebooks aren't running in this runtime".into()), "it goes to the R engine, which this test's runtime hasn't");
        assert!(other.contains("invalid_path::Notebook path must end in .jl (Julia) or .R (R): "), "{other}");
    }
}

#[test]
fn a_new_notebook_without_a_path_is_of_the_sessions_kind() {
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.notebooks.set_kind("r-session", Some(Backend::Ember));
    s.notebooks.set_kind("julia-session", Some(Backend::Pluto));
    let made = s.call("r-session", "new_notebook", json!({}));
    if cfg!(windows) {
        let refused = made.unwrap_err();
        assert!(refused.contains("unsupported::") && refused.contains("R notebooks don't run on Windows yet"), "{refused}");
    } else {
        made.unwrap();
        assert_eq!(*ember.calls.lock().unwrap(), ["new"], "R's engine made it");
    }
    assert!(!s.engine.calls.lock().unwrap().contains(&"new".to_owned()), "not Pluto's");
    s.call("julia-session", "new_notebook", json!({})).unwrap();
    s.call("unsaid", "new_notebook", json!({})).unwrap();
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|call| *call == "new").count(), 2, "a Julia session's, and one whose kind wasn't said, are Pluto's");
    // A path still decides, whatever the session's kind.
    s.notebooks.bind("r-session", "");
    s.call("r-session", "new_notebook", json!({ "path": format!("{}{SEP}endeavor-kind-test.jl", std::env::temp_dir().to_string_lossy().trim_end_matches(SEP)) })).unwrap();
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|call| *call == "new").count(), 3);
    // Forgotten, the session's new notebooks are Pluto's again.
    s.notebooks.set_kind("r-session", None);
    s.notebooks.bind("r-session", "");
    s.call("r-session", "new_notebook", json!({})).unwrap();
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|call| *call == "new").count(), 4);
}

#[test]
fn a_stop_interrupts_the_sessions_own_run() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "sleep(60)")]);
    let interrupts = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "interrupt").count();
    let running = |busy: bool| s.engine.with(NB, |nb| nb.cells[0].running = busy);

    // The cancelled call is known while it is under way.
    assert_eq!(s.notebooks.cancelled("7", &json!(5)), None, "no call under way");
    {
        let _call = s.notebooks.running("7", &json!(5), NB, false);
        assert_eq!(s.notebooks.cancelled("8", &json!(5)), None, "another session's call of the same id");
        assert_eq!(s.notebooks.cancelled("7", &json!(6)), None, "another call");
        assert_eq!(s.notebooks.cancelled("7", &json!(5)), Some((NB.to_owned(), false)));
    }
    assert_eq!(s.notebooks.cancelled("7", &json!(5)), None, "the call ended");

    // A call that runs code is interrupted.
    s.notebooks.stop_for("7", NB, true);
    assert_eq!(interrupts(), 1);

    // Any other call (reading the cell, say): only while a run this session started is going.
    running(true);
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 1, "the user's own run, or another session's, is left alone");
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    running(true);
    s.notebooks.stop_for("8", NB, false);
    assert_eq!(interrupts(), 1, "not another session's Stop");
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "the run session 7 started");
    running(false);
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "once it has ended");
    running(true);
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "and it is forgotten then: this run isn't the session's");

    // A run that ended with no Stop between: a later run of the same cell (the user's, or a
    // reactive one) isn't the session's.
    running(false);
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-2", "cells": [X] } }));
    running(true);
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "the user's later run of a cell the session ran");
    // A run that ends with its cell queued again by a later run (Pluto's own Restart runs every
    // cell): the later run isn't the session's.
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-3", "cells": [] } }));
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "a restart's run of a cell the session ran");
    // A run that ended before the tool that started it recorded it.
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-4", "cells": [X] } }));
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "a quick run that already ended");
    // Its process ended, or the app restarted it: no run of the session's goes on.
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.notified(&json!({ "method": "process_exited", "params": { "notebook_id": NB, "running": [X] } }));
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "after the process ended");
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.restart(NB).unwrap();
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "after the app's Restart");
    // Nor is a waited run that finished within the wait.
    running(false);
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    running(true);
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "a waited run that finished");
}

#[test]
fn interrupt_notebook_stops_the_run_and_says_what_it_stopped() {
    let s = setup();
    const Z: &str = "33333333-3333-3333-3333-333333333333";
    s.engine.open(NB, "/n/a.jl", &[(X, "sleep(60)"), (Y, "y = 1"), (Z, "stubborn()")]);
    let interrupt = || s.call("7", "interrupt_notebook", json!({ "notebook_id": NB })).unwrap();
    let interrupts = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "interrupt").count();

    assert_eq!(interrupt(), json!({ "notebook_id": NB, "interrupted": false, "message": "Nothing was running." }));
    assert_eq!(interrupts(), 0, "nothing was asked of the engine");

    // The user's run, or any session's: the running cell ends interrupted and the queued one doesn't run.
    s.engine.with(NB, |nb| (nb.cells[0].running, nb.cells[1].queued) = (true, true));
    let stopped = interrupt();
    assert_eq!(
        stopped,
        json!({ "notebook_id": NB, "interrupted": true, "stopped": [X], "dropped": [Y], "still_running": [], "process_status": "ready" })
    );
    assert_eq!(interrupts(), 1);
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["running"], &read["errored"]), (&json!(false), &json!(true)));

    // A cell that doesn't stop within the call is named, with what to do.
    s.engine.with(NB, |nb| nb.cells[2].running = true);
    let late = Instant::now() - Duration::from_secs_f64(super::tools::WAIT_SECONDS);
    let stuck = tool_json(s.notebooks.tool("7", "interrupt_notebook", &json!({ "notebook_id": NB }), &Folder::Process, late).unwrap());
    assert_eq!((&stuck["interrupted"], &stuck["stopped"], &stuck["still_running"]), (&json!(true), &json!([]), &json!([Z])));
    assert!(stuck["message"].as_str().unwrap().contains("haven't stopped"), "{stuck}");

    // While the notebook's process starts, the queued cells are dropped at once, and the answer says why (#179).
    s.engine.with(NB, |nb| {
        nb.cells[2].running = false;
        nb.cells[1].code = "starting()".into();
        (nb.cells[0].queued, nb.cells[1].queued) = (true, true);
    });
    let began = Instant::now();
    let starting = interrupt();
    assert!(began.elapsed() < Duration::from_secs(2), "no wait for cells that won't run");
    assert_eq!((&starting["interrupted"], &starting["dropped"], &starting["still_running"]), (&json!(true), &json!([X, Y]), &json!([])), "{starting}");
    assert!(starting["message"].as_str().unwrap().contains("still starting"), "{starting}");
}

#[test]
fn restart_notebook_restarts_and_a_julia_notebooks_run_is_the_sessions() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x")]);
    let interrupts = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "interrupt").count();

    let restarted = s.call("7", "restart_notebook", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&restarted["restarted"], &restarted["process_status"]), (&json!(true), &json!("ready")), "{restarted}");
    assert!(restarted["message"].as_str().unwrap().contains("every cell runs"), "{restarted}");
    // Pluto's restart runs every cell, and that run is the session's: its Stop interrupts it, another's doesn't.
    s.engine.with(NB, |nb| (nb.cells[0].running, nb.cells[1].queued) = (true, true));
    s.notebooks.stop_for("8", NB, false);
    assert_eq!(interrupts(), 0, "another session's Stop");
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 1, "the restart's run is session 7's");
    assert_eq!(restarted.get("running_again"), None, "nothing was running");

    // A cell that was running runs again in the new process: the result names it and says to edit it first.
    s.engine.with(NB, |nb| nb.cells[0].running = true);
    let again = s.call("7", "restart_notebook", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(again["running_again"], json!([X]), "{again}");
    assert!(again["message"].as_str().unwrap().contains("the cell in `running_again` is running again") && again["message"].as_str().unwrap().contains("edit it"), "{again}");
    s.engine.with(NB, |nb| nb.cells[0].running = false);

    // Refused in safe preview, as the page's Restart is.
    s.engine.with(NB, |nb| nb.safe_preview = true);
    let refused = s.call("7", "restart_notebook", json!({ "notebook_id": NB })).unwrap_err();
    assert!(refused.contains("execution_blocked"), "{refused}");

    // Ember's restart leaves every cell not run, and the result says how to run them.
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "x <- 1")]);
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("9", "list_notebooks", json!({})).unwrap();
    let restarted = s.call("9", "restart_notebook", json!({ "notebook_id": R })).unwrap();
    assert_eq!(restarted["restarted"], true);
    assert!(restarted["message"].as_str().unwrap().contains("run_all_cells"), "{restarted}");
}

#[test]
fn an_r_notebooks_package_date_is_shown_previewed_and_moved() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "library(dplyr)")]);
    ember.with(R, |nb| nb.package_date = Some("2026-06-01".into()));
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    let listed = s.call("9", "list_notebooks", json!({})).unwrap();
    assert_eq!(listed.as_array().unwrap().iter().find(|nb| nb["notebook_id"] == R).unwrap()["package_date"], "2026-06-01", "{listed}");

    // Without a date, today: what would change, and the date hasn't moved.
    let preview = s.call("9", "preview_package_update", json!({ "notebook_id": R })).unwrap();
    assert_eq!((&preview["status"], &preview["date"], &preview["current_date"]), (&json!("ready"), &json!("2026-10-17"), &json!("2026-06-01")), "{preview}");
    assert!(preview["message"].as_str().unwrap().contains("apply_package_update"), "{preview}");
    assert_eq!(preview.get("reason"), None, "{preview}");

    // A date that can't be used says why, with Ember's own reason.
    let failed = s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2000-01-01" })).unwrap();
    assert!(failed["status"] == "failed" && failed["reason"] == "no index" && failed["message"].as_str().unwrap().contains("problems"), "{failed}");

    // Applying needs the previewed date's ready preview.
    let refused = s.call("9", "apply_package_update", json!({ "notebook_id": R, "date": "2026-10-17" })).unwrap_err();
    assert!(refused.contains("no_preview"), "{refused}");
    assert!(s.call("9", "apply_package_update", json!({ "notebook_id": R })).unwrap_err().contains("date"), "date is required");
    s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2026-10-17" })).unwrap();
    let applied = s.call("9", "apply_package_update", json!({ "notebook_id": R, "date": "2026-10-17" })).unwrap();
    assert_eq!((&applied["date"], &applied["changes"][0]["to"]), (&json!("2026-10-17"), &json!("1.2.1")), "{applied}");
    let listed = s.call("9", "list_notebooks", json!({})).unwrap();
    assert_eq!(listed.as_array().unwrap().iter().find(|nb| nb["notebook_id"] == R).unwrap()["package_date"], "2026-10-17");

    assert!(applied["message"].as_str().unwrap().contains("installs now"), "{applied}");

    // In safe preview nothing installs until the user lets the notebook run, and the result says so.
    ember.with(R, |nb| nb.safe_preview = true);
    s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2026-10-18" })).unwrap();
    let waiting = s.call("9", "apply_package_update", json!({ "notebook_id": R, "date": "2026-10-18" })).unwrap();
    assert!(waiting["message"].as_str().unwrap().contains("once the user allows the notebook to run"), "{waiting}");

    // A day not published yet is previewed at the newest one that is, and the message says so (#247).
    let earlier = s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2026-10-19" })).unwrap();
    assert_eq!((&earlier["status"], &earlier["date"], earlier.get("asked")), (&json!("ready"), &json!("2026-10-18"), None), "{earlier}");
    assert!(earlier["message"].as_str().unwrap().starts_with("2026-10-19 isn't published yet, so this is the newest day that is, 2026-10-18."), "{earlier}");
    assert!(!preview["message"].as_str().unwrap().contains("published"), "{preview}");

    // A Julia notebook has no package date.
    s.engine.open(NB, "/n/a.jl", &[(Y, "y = 1")]);
    let julia = s.call("7", "preview_package_update", json!({ "notebook_id": NB })).unwrap_err();
    assert!(julia.contains("not_r_notebook"), "{julia}");
}

#[test]
fn the_apps_stop_looks_at_the_notebooks_with_the_sessions_runs() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "sleep(60)")]);
    assert_eq!(s.notebooks.session_runs("7"), Vec::<String>::new());
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!(s.notebooks.session_runs("7"), [NB]);
    assert_eq!(s.notebooks.session_runs("8"), Vec::<String>::new(), "another session's");
    assert_eq!(s.notebooks.session_runs(""), Vec::<String>::new(), "no session");
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-1", "cells": [] } }));
    assert_eq!(s.notebooks.session_runs("7"), Vec::<String>::new(), "the run ended, its cell queued again or not");
}

#[test]
fn a_cell_waiting_on_a_prompt_ends_the_wait_says_what_it_asks_and_takes_an_answer() {
    let s = setup();
    const Z: &str = "33333333-3333-3333-3333-333333333333";
    s.engine.open(NB, "/n/a.R", &[(X, "name = ask()"), (Y, "z = 1"), (Z, "pick = choose()")]);

    // A waited run returns as soon as its cell asks, without spending the wait, and says what it asks.
    let began = Instant::now();
    let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    assert!(began.elapsed() < Duration::from_secs(5), "the run's wait ended at the prompt");
    assert_eq!(ran["waiting_for_input"], json!({ "cell_id": X, "prompt_id": 1, "prompt": "Name? ", "choices": [] }), "{ran}");
    assert_eq!((&ran["execution"]["status"], &ran["execution"]["still_running"]), (&json!("running"), &json!([X])), "{ran}");
    let warnings = ran["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|w| w.as_str().unwrap().starts_with("waiting_for_input::")) && !warnings.iter().any(|w| w.as_str().unwrap().starts_with("execution_timeout::")), "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("answer_prompt") && ran["message"].as_str().unwrap().contains("ask the user first"), "{ran}");

    // read_cell doesn't wait on it either, and shows the prompt.
    let began = Instant::now();
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert!(began.elapsed() < Duration::from_secs(2), "no wait for a cell waiting on a prompt");
    assert_eq!((&read["running"], &read["waiting_for_input"]["prompt"]), (&json!(true), &json!("Name? ")), "{read}");
    assert!(read["message"].as_str().unwrap().contains("answer_prompt"), "{read}");

    // Only the cell waiting can be answered.
    let refused = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": Y, "text": "Ada" })).unwrap_err();
    assert!(refused.contains("no_prompt::"), "{refused}");
    let refused = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": X, "text": 3 })).unwrap_err();
    assert!(refused.contains("invalid_argument::"), "{refused}");
    // An answer for a question the cell has stopped asking isn't given to the one it asks now.
    let answers = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "answer").count();
    let refused = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": X, "text": "Ada", "prompt_id": 7 })).unwrap_err();
    assert!(refused.contains("stale_prompt::"), "{refused}");
    assert_eq!(answers(), 0, "nothing reached the engine");

    // The answer lets the cell finish; a waited answer reports it as a run does.
    let answered = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": X, "text": "Ada", "prompt_id": 1, "wait_for_completion": true })).unwrap();
    assert_eq!(answered["execution"]["status"], json!("completed"), "{answered}");
    assert_eq!(answered["outputs"]["changed"][0]["output_summary"], json!("answered Ada"), "{answered}");
    assert_eq!(answered.get("waiting_for_input"), None);
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["running"], read.get("waiting_for_input")), (&json!(false), None), "{read}");

    // menu()'s choices come too, and an answer not waited for says the run goes on.
    let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": Z, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["waiting_for_input"]["choices"], json!(["Yes", "No"]), "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("a choice's number or its text"), "{ran}");
    let answered = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": Z, "text": "1" })).unwrap();
    assert_eq!(answered["execution"]["status"], json!("running"), "{answered}");

    // An interrupt ends a cell waiting on a prompt, as any running cell.
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    let stopped = s.call("7", "interrupt_notebook", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(stopped["stopped"], json!([X]), "{stopped}");
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["errored"], read.get("waiting_for_input")), (&json!(true), None), "{read}");
}
