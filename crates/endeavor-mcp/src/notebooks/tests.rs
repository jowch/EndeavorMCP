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
    off: bool,
    notes: Vec<String>,
    inputs: Vec<String>,
    prompt: Option<Value>,
    error_kind: Option<&'static str>,
}

impl FakeCell {
    fn new(id: &str, code: &str) -> FakeCell {
        FakeCell { id: id.into(), code: code.into(), folded: false, running: false, queued: false, errored: false, last_run: 0.0, output: String::new(), hidden: false, stale: false, not_run: false, off: false, notes: Vec::new(), prompt: None, inputs: Vec::new(), error_kind: None }
    }

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
    exited: Option<Vec<String>>,
    broken: bool,
    packages: Option<Value>,
    starting: bool,
    runs_first: Vec<String>,
    reruns_besides: Vec<String>,
    lazy: bool,
    autorun: bool,
    package_date: Option<String>,
    previewed: Option<String>,
    stale_on_interrupt: bool,
}

#[derive(Default)]
struct Engine {
    notebooks: Mutex<Vec<FakeNotebook>>,
    clock: Arc<Mutex<f64>>,
    shut_down: Mutex<Vec<String>>,
    hold: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    hold_snapshot: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    seq: Mutex<u64>,
    calls: Mutex<Vec<String>>,
    made: Mutex<u32>,
    waits: Mutex<Vec<f64>>,
    watched: Mutex<u32>,
    hold_open: Mutex<Option<Receiver<()>>>,
    starting: Mutex<()>,
    opens_with: Mutex<Vec<(String, String)>>,
}

impl Engine {
    fn open(&self, id: &str, path: &str, cells: &[(&str, &str)]) {
        let cells = cells.iter().map(|(id, code)| FakeCell::new(id, code)).collect();
        self.notebooks.lock().unwrap().push(FakeNotebook { id: id.into(), path: path.into(), cells, safe_preview: false, exited: None, broken: false, packages: None, starting: false, runs_first: Vec::new(), reruns_besides: Vec::new(), lazy: false, autorun: false, package_date: None, previewed: None, stale_on_interrupt: false });
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
                "last_run": c.last_run, "runtime": 0, "output": c.output, "error": match (c.errored, c.error_kind) { (false, _) => Value::Null, (true, None) => json!({ "msg": c.output }), (true, Some(kind)) => json!({ "kind": kind, "msg": c.output }) }, "hidden": c.hidden, "markdown": c.code.starts_with("md\"") || c.code.starts_with("#'"),
                "stale": c.stale, "not_run": c.not_run, "off": c.off, "notes": c.notes, "prompt": c.prompt, "inputs": c.inputs,
            })).collect::<Vec<_>>(),
        })
    }

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
                let cells = if method == "new" {
                    vec![FakeCell::new(&format!("dddddddd-0000-0000-0000-{:012}", *made), "")]
                } else {
                    let read = std::mem::take(&mut *self.opens_with.lock().unwrap());
                    read.iter().map(|(id, code)| FakeCell { queued: params["run"] == true, ..FakeCell::new(id, code) }).collect()
                };
                let listed: Vec<Value> = cells.iter().map(|c| json!({ "cell_id": c.id, "code": c.code })).collect();
                let lazy = method == "open" && path.contains("lazy");
                notebooks.push(FakeNotebook { id: id.clone(), path: path.clone(), cells, safe_preview: params["run"] == false, exited: None, broken: false, packages: None, starting: false, runs_first: Vec::new(), reruns_besides: Vec::new(), lazy, autorun: false, package_date: None, previewed: None, stale_on_interrupt: false });
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
            "preview_packages" => {
                if let Some(package) = params["package"].as_str() {
                    if package != "dplyr" {
                        return Err(format!("ArgumentError: not_updatable::{package} is not in this notebook's packages"));
                    }
                    if params["version"] == "0.0.1" {
                        nb.previewed = None;
                        return Ok(json!({ "date": null, "asked": null, "current_date": nb.package_date, "status": "failed", "changes": [], "problems": [], "message": "CRAN has no dplyr 0.0.1" }));
                    }
                    nb.previewed = Some("2023-09-02".into());
                    return Ok(json!({ "date": "2023-09-02", "asked": null, "current_date": nb.package_date, "status": "ready", "problems": [], "message": null,
                        "changes": [{ "name": "dplyr", "from": "1.1.4", "to": "1.1.2", "change": "downgraded" }, { "name": "rlang", "from": "1.1.4", "to": "1.1.1", "change": "downgraded" }] }));
                }
                let date = params["date"].as_str().unwrap_or("2026-10-17").to_owned();
                if date.starts_with("2000") {
                    nb.previewed = None;
                    return Ok(json!({ "date": date, "current_date": nb.package_date, "status": "failed", "changes": [],
                        "problems": [{ "kind": "index_unavailable", "package": null, "message": "no package index for 2000-01-01" }], "message": "no index" }));
                }
                if date == "2026-10-16" || date == "2025-01-15" {
                    nb.previewed = Some(date.clone());
                    return Ok(json!({ "date": date, "asked": date, "current_date": nb.package_date, "status": "ready", "changes": [], "problems": [], "message": null,
                        "packages": if date == "2025-01-15" { 0 } else { 1 } }));
                }
                let asked = date.clone();
                let date = if date == "2026-10-19" { "2026-10-18".to_owned() } else { date };
                nb.previewed = Some(date.clone());
                Ok(json!({ "date": date, "asked": asked, "current_date": nb.package_date, "status": "ready", "changes": [{ "name": "dplyr", "from": "1.1.4", "to": "1.2.1", "change": "upgraded" }], "problems": [], "message": null }))
            }
            "set_input" => {
                let name = params["name"].as_str().unwrap();
                let Some(input) = nb.cells.iter().find(|c| c.code.starts_with(&format!("{name} <- input_"))).map(|c| c.id.clone()) else {
                    return Err(format!("ArgumentError: input_refused::No input is bound to `{name}`."));
                };
                if params["value"] == "bad" {
                    return Err("ArgumentError: invalid_value::The value must be a number from 1 to 10.".into());
                }
                if params["value"] == "late" {
                    return Ok(json!({ "accepted": true, "pending": true, "process_status": "running", "cell": input, "cells": [], "lazy": nb.lazy }));
                }
                let readers: Vec<String> = nb.cells.iter().filter(|c| c.id != input && c.analysis().1.iter().any(|n| n == name)).map(|c| c.id.clone()).collect();
                let mut names: Vec<String> = Vec::new();
                for cell in nb.cells.iter_mut().filter(|c| readers.contains(&c.id)) {
                    if nb.lazy {
                        cell.stale = true;
                    } else {
                        cell.last_run = now;
                        cell.output = format!("ran {}", cell.code);
                        names.extend(cell.analysis().0);
                    }
                }
                if nb.autorun {
                    for cell in nb.cells.iter_mut().filter(|c| c.id != input && !readers.contains(&c.id) && !c.not_run && !c.off && c.last_run > 0.0) {
                        let (defs, refs) = cell.analysis();
                        if refs.iter().any(|n| names.contains(n)) {
                            names.extend(defs);
                            cell.last_run = now;
                            cell.output = format!("ran {}", cell.code);
                        }
                    }
                }
                let mut reply = json!({ "accepted": true, "process_status": "ready", "cell": input, "value": params["value"], "cells": readers, "lazy": nb.lazy });
                if params["wait"] == true && !nb.lazy {
                    reply["completed"] = json!(readers);
                    reply["timed_out"] = json!([]);
                }
                Ok(reply)
            }
            "set_package_date" => {
                if nb.previewed.as_deref() != params["date"].as_str() {
                    return Err("ArgumentError: no_preview::no ready, current preview for that date".into());
                }
                nb.package_date = nb.previewed.take();
                Ok(json!({ "date": nb.package_date, "changes": [{ "name": "dplyr", "from": "1.1.4", "to": "1.2.1", "change": "upgraded" }], "process_status": if nb.safe_preview { "waiting_for_permission" } else { "ready" } }))
            }
            "approve_install" => {
                let Some(asking) = nb.packages.take_if(|p| p["step"] == "asking") else {
                    return Err("ArgumentError: nothing_to_approve::no install is waiting for an OK".into());
                };
                nb.packages = Some(json!({ "step": "installing", "packages": asking["packages"], "seconds": 0, "last_line": null }));
                Ok(json!({ "approved": true, "process_status": "ready" }))
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
                if nb.packages.as_ref().is_some_and(|p| p["step"] == "asking") {
                    for cell in nb.cells.iter_mut().filter(|c| cells.contains(&c.id)) {
                        cell.queued = true;
                    }
                    let mut watched = self.watched.lock().unwrap();
                    *watched += 1;
                    let mut reply = json!({ "accepted": true, "process_status": "ready", "run": format!("run-{watched}") });
                    if params["wait"] == true {
                        reply["completed"] = json!([]);
                        reply["timed_out"] = json!(cells);
                    }
                    return Ok(reply);
                }
                if params["ask_new"] == true && nb.cells.iter().any(|c| cells.contains(&c.id) && c.code.contains("using Example")) {
                    for cell in nb.cells.iter_mut().filter(|c| cells.contains(&c.id)) {
                        cell.queued = true;
                    }
                    nb.packages = Some(json!({ "step": "asking", "new": true, "packages": ["Example"], "seconds": 0, "last_line": null, "download": { "bytes": null, "packages": [{ "package": "Example" }] } }));
                    return Ok(json!({ "accepted": true, "asking": ["Example"], "process_status": "ready", "completed": [], "timed_out": cells }));
                }
                if let Some(cell) = nb.cells.iter().find(|c| cells.contains(&c.id) && c.code.contains("crash()")) {
                    nb.exited = Some(vec![cell.id.clone()]);
                    let mut reply = json!({ "accepted": true, "process_status": "ready" });
                    if params["wait"] == true {
                        reply = json!({ "accepted": true, "process_status": "no_process", "completed": cells, "timed_out": [], "exited": [cell.id] });
                    }
                    return Ok(reply);
                }
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
                if params["run"] == true && params["ask_new"] == true && nb.cells.iter().any(|c| c.code.contains("using Example")) {
                    nb.cells.iter_mut().for_each(|c| c.queued = true);
                    nb.packages = Some(json!({ "step": "asking", "new": true, "packages": ["Example"], "seconds": 0, "last_line": null, "download": { "bytes": null, "packages": [{ "package": "Example" }] } }));
                    return Ok(json!({ "already_allowed": false, "ran": true, "asking": ["Example"], "process_status": "ready" }));
                }
                nb.starting = params["run"] == true;
                let mut reply = json!({ "already_allowed": false, "ran": params["run"], "process_status": if params["run"] == true { "starting" } else { "ready" } });
                if params["run"] == true && nb.cells.iter().any(|c| c.code.contains("linger()")) {
                    nb.cells.iter_mut().filter(|c| c.code.contains("linger()")).for_each(|c| c.running = true);
                    let mut watched = self.watched.lock().unwrap();
                    *watched += 1;
                    reply["run"] = json!(format!("run-{watched}"));
                }
                Ok(reply)
            }
            "render_text" => Ok({
                let at = find(nb, &params["cell_id"]);
                let code = &nb.cells[at].code;
                json!({ "text": if code.contains("wide table") { json!("n\t".repeat(300)) } else if code.contains("table") { json!("n\tmean\n3\t2.5") } else { Value::Null } })
            }),
            "render_png" => Ok({
                let at = find(nb, &params["cell_id"]);
                let code = &nb.cells[at].code;
                if code.contains("widget") {
                    json!({ "png": "iVBORw==", "mime": "text/html", "source": "embedded image" })
                } else if code.contains("scripted html") {
                    json!({ "png": Value::Null, "mime": "text/html", "interactive": true })
                } else if code.contains("plotly") {
                    json!({ "png": "iVBORw==", "mime": "text/html", "interactive": true })
                } else if code.contains("html") {
                    json!({ "png": Value::Null, "mime": "text/html" })
                } else if code.contains("wrapped plot") {
                    json!({ "png": "iVBORw==", "mime": "text/html" })
                } else if code.contains("gr plot") {
                    json!({ "png": "iVBORw==", "mime": "image/png" })
                } else {
                    json!({ "png": if code.contains("plot") { json!("iVBORw==") } else { Value::Null }, "mime": "text/plain" })
                }
            }),
            "validate" => Ok(json!({ "errors": if params["code"].as_str().unwrap().contains('\n') { json!([{ "type": "pluto_multi_expression" }]) } else { json!([]) } })),
            "interrupt" => {
                if nb.cells.iter().any(|c| c.queued && c.code.contains("starting()")) {
                    return Ok(json!({ "interrupted": true, "pending": true }));
                }
                let busy = nb.cells.iter().any(|c| c.running);
                let stale = nb.stale_on_interrupt;
                for cell in nb.cells.iter_mut() {
                    cell.stale |= stale && cell.queued;
                    cell.queued = false;
                    if cell.running && !cell.code.contains("stubborn()") {
                        (cell.running, cell.errored, cell.output, cell.prompt) = (false, true, "InterruptException".into(), None);
                    }
                }
                Ok(json!({ "interrupted": busy }))
            }
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

    s.read("7", NB, Y);
    s.edit("7", NB, Y, "y = x * 8");
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!((&cell(&event, NB, Y)["author"], &cell(&event, NB, Y)["before"], &cell(&event, NB, Y)["unrun"]), (&json!("agent"), &json!("y = x * 7"), &json!(true)));
    assert_eq!(cell(&event, NB, Y)["version"], format!("{:x}", hash("y = x * 8")));
    assert_eq!(event["notebooks"][0]["pending_run"], json!([Y]));

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

    s.engine.with(NB, |nb| nb.cells[0].code = "x = 5".into());
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, X)["author"], "user");

    let cells = |code: &str| json!({ "method": "cell_state", "params": { "notebook_id": NB, "cells": [{ "cell_id": Y, "code": code }] } });
    assert!(s.notebooks.notified(&cells("y = 1")));
    assert!(s.notebooks.notified(&cells("y = x * 10")));
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, Y)["author"], "user");

    let z = s.call("7", "add_cell", json!({ "notebook_id": NB, "after_cell_id": Y, "code": "z = y + 1" })).unwrap()["cell_id"].as_str().unwrap().to_owned();
    s.read("7", NB, X);
    s.call("7", "edit_cells", json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 1" }] })).unwrap();
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!(cell(&event, NB, &z), &json!({ "cell_id": z, "running": false, "errored": false, "unrun": true, "author": "agent", "before": "", "version": format!("{:x}", hash("z = y + 1")), "name": "z" }));
    assert_eq!((&cell(&event, NB, X)["author"], &cell(&event, NB, X)["before"]), (&json!("agent"), &json!("x = 5")));

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
fn a_notebook_that_closes_while_the_events_are_put_together_is_left_out() {
    let s = setup();
    let (a, b) = (id(1), id(2));
    s.engine.open(&a, "/n/a.jl", &[(X, "x = 1")]);
    s.engine.open(&b, "/n/b.jl", &[(X, "x = 1")]);
    let (read_tx, read) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *s.engine.hold_snapshot.lock().unwrap() = Some((read_tx, release_rx));
    let subscribing = std::thread::spawn({
        let notebooks = s.notebooks.clone();
        move || notebooks.subscribe().map(|(first, _)| first)
    });
    read.recv().unwrap();
    s.engine.notebooks.lock().unwrap().retain(|nb| nb.id != a);
    release.send(()).unwrap();
    let first: Value = serde_json::from_str(&subscribing.join().unwrap().expect("the events start")).unwrap();
    let listed: Vec<&Value> = first["notebooks"].as_array().unwrap().iter().map(|nb| &nb["notebook_id"]).collect();
    assert_eq!(listed, [&json!(b)], "{first}");
}

#[test]
fn a_read_of_the_notebook_from_before_an_agent_edit_doesnt_make_it_the_users() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 6"), (Y, "y = x * 7")]);
    let (_, rx) = s.notebooks.subscribe().unwrap();
    s.read("7", NB, X);

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
    assert_eq!((&receipt["cell_order"], &receipt["execution_order"], &receipt["code"]), (&Value::Null, &Value::Null, &Value::Null));
    assert_eq!(s.engine.code(NB, X), "x = 10");
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([X]), "listing counts as no read");

    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["pending_run"], &ran["execution"]["status"]), (&json!([X]), &json!([]), &json!("completed")));
    assert_eq!(ran["outputs"]["changed"], json!([{ "cell_id": X, "output_summary": "ran x = 10" }]));
    let noop = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&noop["affected_cells"], &noop["execution"]["status"]), (&json!([]), &json!("completed")));

    assert_eq!(
        s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [Y] })),
        Err(format!("ArgumentError: not_staged::Cell {Y} is not in pending_run; stage first or pass force=true"))
    );
    let forced = s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [Y], "force": true, "wait_for_completion": true })).unwrap();
    assert_eq!(forced["affected_cells"], json!([Y]));
    assert_eq!(s.refused("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": ["bad"] })), "invalid_cell_id");

    s.seconds(1.0);
    s.edit("", NB, X, "x = 11");
    let receipt = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!(receipt["execution"]["status"], "running");
    assert_eq!(receipt["warnings"], json!(["async_execution::cells running; read them for the result"]));
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
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1")]);
    s.engine.with(NB, |nb| (nb.cells[0].output, nb.cells[0].errored) = ("boom".into(), true));
    s.read("", NB, X);
    let receipt = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = linger()", "run_after": true })).unwrap();
    assert_eq!((&receipt["running"], &receipt["previous_output"], receipt.get("output")), (&json!(true), &json!("boom"), None), "{receipt}");
    assert_eq!((&receipt["previous_errored"], &receipt["previous_error"], receipt.get("errored"), receipt.get("error")), (&json!(true), &json!({ "msg": "boom" }), None, None), "the old error isn't the edit's: {receipt}");
    assert_eq!(receipt["outputs"]["changed"], json!([]), "its old output isn't a change");

    s.engine.with(NB, |nb| (nb.cells[0].running, nb.cells[0].output, nb.cells[0].errored) = (false, "2".into(), false));
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["output"], &read["errored"], read.get("error"), read.get("previous_output")), (&json!("2"), &json!(false), None, None));
}

#[test]
fn a_staged_edit_gives_the_old_run_as_previous() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1")]);
    s.engine.with(NB, |nb| (nb.cells[0].output, nb.cells[0].errored) = ("boom".into(), true));
    s.read("", NB, X);
    let receipt = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 2" })).unwrap();
    assert_eq!((&receipt["stale"], &receipt["previous_output"], receipt.get("output")), (&json!(true), &json!("boom"), None), "{receipt}");
    assert_eq!((&receipt["previous_errored"], &receipt["previous_error"], receipt.get("errored"), receipt.get("error")), (&json!(true), &json!({ "msg": "boom" }), None, None), "the old error isn't the edit's: {receipt}");
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["errored"], &read["stale"], read.get("previous_errored")), (&json!(true), &json!(true), None), "{read}");
}

#[test]
fn a_staged_edit_keeps_the_notes_on_its_own_code_as_notes() {
    let s = setup();
    s.engine.open(NB, "/n/a.py", &[(X, "mo.md(\"## a\")")]);
    s.read("", NB, X);
    s.engine.with(NB, |nb| nb.cells[0].notes = vec!["printed::old run".into(), "markdown_in_file::marimo writes it in another form".into()]);
    let receipt = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "mo.md(\"C:\\\\\")" })).unwrap();
    assert_eq!((&receipt["notes"], &receipt["previous_notes"]), (&json!(["markdown_in_file::marimo writes it in another form"]), &json!(["printed::old run"])), "{receipt}");
    s.engine.with(NB, |nb| nb.cells[0].notes = vec!["markdown_in_file::marimo writes it in another form".into()]);
    let receipt = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "mo.md(\"D:\\\\\")" })).unwrap();
    assert_eq!(receipt.get("previous_notes"), None, "{receipt}");
}

#[test]
fn a_run_in_lazy_mode_says_so_and_names_the_stale_cells() {
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1")]);
    s.engine.with(NB, |nb| nb.lazy = true);
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["on_cell_change"], "lazy");
    s.read("", NB, X);
    s.edit("", NB, X, "x = 2");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!((&ran["affected_cells"], &ran["stale_cells"]), (&json!([X]), &json!([Y])), "{ran}");
    assert!(ran["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().starts_with("lazy_mode::")), "{ran}");

    s.engine.with(NB, |nb| (nb.lazy, nb.cells[1].stale) = (false, false));
    s.edit("", NB, X, "x = 3");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert!(ran.get("stale_cells").is_none() && !ran["warnings"].to_string().contains("lazy_mode"), "{ran}");
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0].get("on_cell_change"), None);
}

#[test]
fn a_run_names_the_dependents_it_reruns_and_a_waited_one_reports_them() {
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1"), ("33333333-3333-3333-3333-333333333333", "z = error(y)"), ("44444444-4444-4444-4444-444444444444", "w = y"), ("55555555-5555-5555-5555-555555555555", "v = 2")]);
    let (z, w, v) = ("33333333-3333-3333-3333-333333333333", "44444444-4444-4444-4444-444444444444", "55555555-5555-5555-5555-555555555555");
    s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.engine.with(NB, |nb| {
        nb.autorun = true;
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

    s.seconds(1.0);
    let edited = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 3", "run_after": true })).unwrap();
    assert_eq!(edited["execution"]["will_run"], json!([Y, z]), "{edited}");
    assert!(!edited["execution"]["will_run"].to_string().contains(v) && !edited["execution"]["will_run"].to_string().contains(w));

    s.engine.with(NB, |nb| nb.lazy = true);
    s.seconds(1.0);
    let edited = s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 4", "run_after": true })).unwrap();
    assert_eq!(edited["execution"].get("will_run"), None, "{edited}");
}

#[test]
fn a_waited_run_cuts_a_dependents_output_text_shorter_than_its_own_cells() {
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1 # wide table"), (Y, "y = x # wide table")]);
    s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.engine.with(NB, |nb| nb.autorun = true);
    s.seconds(1.0);
    s.read("", NB, X);
    s.edit("", NB, X, "x = 2 # wide table");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    let text = |cell: &str| ran["outputs"]["changed"].as_array().unwrap().iter().find(|c| c["cell_id"] == cell).unwrap_or_else(|| panic!("{ran}"))["output_text"].as_str().unwrap().to_owned();
    assert_eq!(text(X), "n\t".repeat(300), "the run's own cell, in full up to 2000 bytes");
    assert!(text(Y).starts_with(&"n\t".repeat(100)) && text(Y).ends_with("… (cut; read_cell shows more)") && text(Y).len() < 250, "{}", text(Y));
    s.seconds(1.0);
    s.read("", NB, Y);
    s.edit("", NB, Y, "y = x + 1 # wide table");
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    let y = ran["outputs"]["changed"].as_array().unwrap().iter().find(|c| c["cell_id"] == Y).unwrap_or_else(|| panic!("{ran}")).clone();
    assert_eq!(y["output_text"], json!("n\t".repeat(300)), "{y}");
    s.seconds(1.0);
    s.read("", NB, Y);
    s.edit("", NB, Y, "y = error(x) # wide table");
    s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.seconds(1.0);
    s.read("", NB, X);
    s.edit("", NB, X, "x = 3 # wide table");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    let y = ran["outputs"]["changed"].as_array().unwrap().iter().find(|c| c["cell_id"] == Y).unwrap_or_else(|| panic!("{ran}")).clone();
    assert!(y.get("error").is_some() && y.get("output_text").is_none_or(|text| *text == json!("n\t".repeat(300))), "{y}");
}

#[test]
fn a_disabled_dependent_or_a_dependent_of_one_is_not_named_as_what_will_run() {
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1"), ("33333333-3333-3333-3333-333333333333", "z = y + 1"), ("44444444-4444-4444-4444-444444444444", "w = x + 1")]);
    let (z, w) = ("33333333-3333-3333-3333-333333333333", "44444444-4444-4444-4444-444444444444");
    s.call("7", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    s.engine.with(NB, |nb| {
        nb.cells[1].off = true;
        nb.cells[2].off = true;
    });
    s.seconds(1.0);
    s.read("7", NB, X);
    let edited = s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = linger()", "run_after": true })).unwrap();
    assert_eq!(edited["execution"]["will_run"], json!([w]), "{edited}");
    let read = |cell: &str, began: Instant| super::tools::tool_json(s.notebooks.tool("7", "read_cell", &json!({ "notebook_id": NB, "cell_id": cell }), &Folder::Process, began).unwrap());
    let began = Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    let off = read(z, began);
    assert!(off["output"] == "ran z = y + 1" && off.get("message").is_none(), "a read of an off cell doesn't wait for the run: {off}");
    assert!(read(w, began).get("message").is_some(), "a read of the dependent that will run waits for it");
}

#[test]
fn lazy_mode_doesnt_list_an_off_cell_as_stale_to_run() {
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

    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    assert!(!ran["warnings"].to_string().contains("changed_in_place"), "{ran}");
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap().get("notes"), None);
}

#[test]
fn what_a_julia_cell_printed_and_logged_reaches_the_agent() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + 1")]);
    let notes = ["logged_warning::3 rows dropped (n = 3)", "logged_info::loaded", "printed::fitting\ndone", "markdown_in_file::marimo writes this markdown cell in another form"];
    s.engine.with(NB, |nb| nb.cells.iter_mut().for_each(|c| c.notes = notes.iter().map(|n| n.to_string()).collect()));
    s.read("", NB, X);
    s.engine.with(NB, |nb| nb.reruns_besides = vec![Y.into()]);
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    let warnings: Vec<&str> = ran["warnings"].as_array().unwrap().iter().filter_map(|w| w.as_str()).collect();
    for note in &notes[1..] {
        let (kind, text) = note.split_once("::").unwrap();
        assert!(warnings.contains(&format!("{kind}::Cell {X}: {text}").as_str()), "{ran}");
    }
    assert!(ran["message"].as_str().unwrap().contains("tell the user what it says"), "{ran}");
    assert!(warnings.contains(&format!("logged_warning::Cells {X}, {Y}: 3 rows dropped (n = 3)").as_str()), "{ran}");
    assert!(!warnings.iter().any(|w| w.contains(Y) && (w.starts_with("printed::") || w.starts_with("logged_info::") || w.starts_with("markdown_in_file::"))), "{ran}");
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(read["notes"], json!(notes), "{read}");
    assert!(read["message"].as_str().unwrap().starts_with(&format!("Cell {Y} logged a warning: \"3 rows dropped (n = 3)\"")), "{read}");
}

#[test]
fn a_name_defined_in_two_cells_says_where_the_other_is() {
    const Z: &str = "33333333-3333-3333-3333-333333333333";
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "x = 2"), (Z, "z = 3")]);
    let error = "Multiple definitions for x.\n\nCombine all definitions into a single reactive cell using a `begin ... end` block.";
    s.engine.with(NB, |nb| nb.cells.iter_mut().filter(|c| c.id != Z).for_each(|c| (c.errored, c.output) = (true, error.into())));
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    let fix = read["defined_elsewhere"].as_str().unwrap_or_default();
    assert!(fix.starts_with(&format!("`x` is also defined in {X}.")) && fix.contains("keep its `using`"), "{read}");
    s.engine.with(NB, |nb| nb.cells.iter_mut().for_each(|c| (c.errored, c.output) = (c.id == Z, if c.id == Z { "UndefVarError: `q` not defined".into() } else { String::new() })));
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Z })).unwrap().get("defined_elsewhere"), None);
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "x = 1"), (Y, "x = 2")]);
    ember.with(R, |nb| nb.cells.iter_mut().for_each(|c| (c.errored, c.output) = (true, error.into())));
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("", "list_notebooks", json!({})).unwrap();
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": R, "cell_id": Y })).unwrap().get("defined_elsewhere"), None);
}

#[test]
fn an_r_package_that_failed_to_install_says_to_stop_and_tell_the_user() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000003";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/genome.R", &[(X, "library(BSgenome.Mmusculus.UCSC.mm10)"), (Y, "library(ggplot2)")]);
    let failed = "there is no package called 'BSgenome.Mmusculus.UCSC.mm10': install failed:\nERROR: lazy loading failed for package 'BSgenome.Mmusculus.UCSC.mm10'";
    ember.with(R, |nb| {
        nb.cells[0].errored = true;
        nb.cells[0].output = failed.into();
        nb.cells[0].error_kind = Some("missing_package");
        nb.cells[1].errored = true;
        nb.cells[1].output = "there is no package called 'svglite'".into();
        nb.cells[1].error_kind = Some("missing_package");
    });
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("", "list_notebooks", json!({})).unwrap();
    let read = s.call("", "read_cell", json!({ "notebook_id": R, "cell_id": X })).unwrap();
    let said = read["install_failed"].as_str().unwrap_or_default();
    assert_eq!(said, crate::notebooks::tools::INSTALL_FAILED, "{read}");
    assert!(said.contains("don't rerun") && said.contains("install the package from a cell yourself") && said.contains("ask you to try again, run the cell once more") && said.contains("didn't compute it"), "{said}");
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": R, "cell_id": Y })).unwrap().get("install_failed"), None);
}

#[test]
fn a_julia_cells_controls_and_a_result_from_a_moved_one_reach_the_agent() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "@bind w Slider(1:10, default=4)"), (Y, "y = w * 2")]);
    let note = "uses_moved_input::This result used w = 9, set on the page. The file's default is 4, so a rerun of the file gives other results.";
    s.engine.with(NB, |nb| {
        nb.cells[0].inputs = vec!["w: Slider 1 to 10, step 1, value 9 (default 4)".into()];
        nb.cells[1].notes = vec![note.into()];
    });
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!(read["inputs"], json!(["w: Slider 1 to 10, step 1, value 9 (default 4)"]), "{read}");
    assert_eq!(read.get("message"), None, "{read}");
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!((read.get("inputs"), &read["notes"]), (None, &json!([note])), "{read}");
    let message = read["message"].as_str().unwrap();
    assert!(message.starts_with(&format!("Cell {Y}: This result used w = 9")) && message.contains("say which value it used"), "{read}");

    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true })).unwrap();
    assert!(ran["warnings"].as_array().unwrap().contains(&json!(format!("uses_moved_input::Cell {Y}: This result used w = 9, set on the page. The file's default is 4, so a rerun of the file gives other results."))), "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("say which value it used"), "{ran}");
}

#[test]
fn a_runs_notes_are_folded_and_capped() {
    let s = setup();
    let ids: Vec<String> = (0..14).map(|i| format!("{i:08}-0000-0000-0000-000000000000")).collect();
    let cells: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "x")).collect();
    s.engine.open(NB, "/n/a.jl", &cells);
    s.engine.with(NB, |nb| {
        for (i, c) in nb.cells.iter_mut().enumerate() {
            c.notes = vec![format!("printed::{}", format!("line {i} ").repeat(60)), "logged_warning::2 plots have no yield recorded and were left out".into()];
        }
    });
    let ran = s.call("", "run_all_cells", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    let warnings: Vec<&str> = ran["warnings"].as_array().unwrap().iter().filter_map(|w| w.as_str()).collect();
    let notes: Vec<&&str> = warnings.iter().filter(|w| w.starts_with("printed::") || w.starts_with("logged_")).collect();
    assert_eq!(notes[0], &format!("logged_warning::Cells {}: 2 plots have no yield recorded and were left out", ids.join(", ")), "{ran}");
    assert_eq!(warnings.iter().filter(|w| w.starts_with("logged_warning::")).count(), 1, "{ran}");
    assert!(notes.len() <= 10 && notes.iter().map(|n| n.len()).sum::<usize>() <= 4_000, "{ran}");
    let cut = warnings.iter().find(|w| w.starts_with("notes_cut::")).unwrap_or_else(|| panic!("{ran}"));
    assert!(cut.contains("more notes from this run are not shown; read_cell"), "{cut}");
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": ids[13] })).unwrap();
    assert_eq!(read["notes"].as_array().unwrap().len(), 2, "{read}");
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

    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before + 1);
    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!(runs(), before + 2);

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

    s.read("", NB, X);
    s.edit("", NB, X, "x = 2");
    s.edit("", NB, Y, "y = x + 4");
    users_run(&s);
    s.seconds(1.0);
    s.edit("", NB, X, "x = 3");
    s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [X, Y], "force": true })).unwrap();
    assert_eq!(runs(), before + 3);

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

    let was = last_run(&s);
    users_run(&s);
    run_anyway(&s, was);
    let receipt = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before, "not run again");
    assert_eq!(receipt["warnings"], json!([format!("already_ran::{Y} already ran after the user's change; not run again, so it ran once.")]));
    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before + 1, "asked again, it runs");

    run_anyway(&s, last_run(&s));
    s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!(runs(), before + 2);

    let was = last_run(&s);
    run_anyway(&s, was);
    users_run(&s);
    s.seconds(120.0);
    s.call("", "submit_changes", json!({ "notebook_id": NB, "cell_ids": [Y], "force": true })).unwrap();
    assert_eq!(runs(), before + 3);
}

#[test]
fn a_read_says_when_someone_else_changed_the_cell_since_this_session_read_it() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "rate = 0.3"), (Y, "y = 2")]);
    let read = |owner: &str| s.call(owner, "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let edit = |owner: &str, code: &str| s.call(owner, "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": code })).unwrap();
    s.read("a", NB, X);
    s.read("b", NB, X);
    edit("b", "rate = 0.4");
    let after = read("a");
    assert_eq!(after["changed_since_your_read"], json!({ "was": "rate = 0.3" }), "{after}");
    assert!(after["message"].as_str().unwrap().contains("Tell the user what changed"), "{after}");
    assert_eq!(read("a").get("changed_since_your_read"), None, "said once");
    edit("a", "rate = 0.5");
    assert_eq!(read("a").get("changed_since_your_read"), None);
    s.engine.with(NB, |nb| nb.cells[0].code = "rate = 0.6".into());
    let code = s.call("a", "read_notebook_code", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(code["changed_since_your_read"], json!({ X: { "was": "rate = 0.5" } }), "{code}");
    assert!(code["message"].as_str().unwrap().contains("Tell the user what changed"), "{code}");
    assert_eq!(s.call("c", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap().get("changed_since_your_read"), None);
    s.read("", NB, X);
    s.engine.with(NB, |nb| nb.cells[0].code = "rate = 0.7".into());
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap().get("changed_since_your_read"), None);
}

#[test]
fn a_first_read_says_when_another_session_changed_the_cell_a_moment_ago() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "rate = 0.3"), (Y, "y = 2")]);
    let read = |owner: &str| s.call(owner, "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.read("b", NB, X);
    s.call("b", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "rate = 0.4" })).unwrap();
    s.seconds(30.0);
    let first = read("a");
    assert_eq!(first["edited_by_another_session"], json!({ "seconds_ago": 30 }), "{first}");
    assert!(first["message"].as_str().unwrap().contains("Another Endeavor session changed this cell 30 s ago"), "{first}");
    assert_eq!(read("a").get("edited_by_another_session"), None, "said once");
    let code = s.call("c", "read_notebook_code", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(code["edited_by_another_session"], json!({ X: { "seconds_ago": 30 } }), "{code}");
    assert!(code["message"].as_str().unwrap().contains("Tell the user before you replace those changes"), "{code}");
    assert_eq!(read("b").get("edited_by_another_session"), None);
    s.seconds(600.0);
    assert_eq!(read("d").get("edited_by_another_session"), None, "ten minutes on, it is the notebook as it is");
    s.engine.with(NB, |nb| nb.cells[1].code = "y = 3".into());
    assert_eq!(s.call("e", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap().get("edited_by_another_session"), None);
    s.read("b", NB, Y);
    s.call("b", "edit_cell", json!({ "notebook_id": NB, "cell_id": Y, "code": "y = 4" })).unwrap();
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap().get("edited_by_another_session"), None);
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
    assert_eq!(s.call("", "edit_cell", edit("x = 4")), Err(format!("ArgumentError: stale_read::Cell {X} changed since you last read it, {}. Read it again (read_cell), and tell the user about that change before you replace it", super::tools::CHANGED_BY)));
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"]), (&json!("x = 99"), &Value::Null));
    s.call("", "read_notebook_code", json!({ "notebook_id": NB })).unwrap();
    s.call("", "edit_cell", edit("x = 4")).unwrap();
    assert_eq!(s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X })), Err("KeyError: key \"code\" not found".into()));
    assert_eq!(s.refused("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 5", "run_after": "yes" })), "invalid_argument");

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

    let folded = || s.engine.with(NB, |nb| (nb.cells[0].folded, nb.cells[1].folded));
    let bad = json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 11", "folded": true }, { "cell_id": Y, "code": "y = 21", "folded": "yes" }] });
    assert_eq!(s.refused("", "edit_cells", bad), "invalid_argument");
    assert_eq!((s.engine.code(NB, X), folded()), ("x = 10".into(), (false, false)), "a refused batch changes no cell");
    s.engine.with(NB, |nb| nb.cells[1].folded = true);
    let receipt = s.call("", "edit_cells", json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 11", "folded": true }, { "cell_id": Y, "code": "y = 21" }] })).unwrap();
    assert_eq!((s.engine.code(NB, X), s.engine.code(NB, Y), folded()), ("x = 11".into(), "y = 21".into(), (true, true)));
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
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"], &read["code_sha"]), (&Value::Null, &json!(true), &json!("b5bc1ffd9091")));
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a, "include_code": true })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"]), (&json!("a = 1"), &Value::Null));
    assert_eq!(s.call("9", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap()["code"], "a = 1");
    for _ in 0..11 {
        s.call("", "list_notebooks", json!({})).unwrap();
    }
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": a })).unwrap();
    assert_eq!((&read["code"], &read["code_unchanged"]), (&json!("a = 1"), &Value::Null));
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
    let b = add(json!({ "notebook_id": NB, "code": "b = a", "after_cell_id": a, "folded": true })).unwrap();
    assert_eq!((&b["code_folded"], &b["execution"]["status"]), (&json!(true), &json!("staged")));
    let b = b["cell_id"].as_str().unwrap().to_owned();
    let c = add(json!({ "notebook_id": NB, "code": 42, "after_cell_id": a, "run_after": true })).unwrap();
    assert_eq!((&c["code"], &c["affected_cells"], &c["execution"]["status"]), (&json!("42"), &json!([c["cell_id"]]), &json!("running")), "the code as the engine has it, since it isn't what was sent");
    let c = c["cell_id"].as_str().unwrap().to_owned();
    assert_eq!(s.engine.code(NB, &c), "42");
    assert_eq!(s.engine.order(NB), [a.clone(), c.clone(), b.clone()]);
    s.edit("", NB, &b, "b = a + 1");
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
    let refused = s.call("7", "submit_changes", json!({ "notebook_id": NB }));
    assert!(refused.as_ref().is_err_and(|e| e.contains(&format!("run_conflict::{X} changed since this session last read them"))), "{refused:?}");
    s.read("7", NB, X);
    s.call("7", "submit_changes", json!({ "notebook_id": NB, "wait_for_completion": true })).unwrap();
    assert_eq!(pending(), json!([]));
    s.notebooks.publish();

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

    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap();
    assert_eq!((&read["running"], &read["stale"]), (&json!(true), &json!(false)));
    let runs = || s.engine.calls.lock().unwrap().iter().filter(|call| *call == "run").count();
    let before = runs();
    let again = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!((&again["affected_cells"], &again["execution"]["status"], runs()), (&json!([]), &json!("completed"), before));

    s.seconds(5.0);
    let now = *s.clock.lock().unwrap();
    s.engine.with(NB, |nb| {
        nb.cells[1].running = false;
        nb.cells[1].last_run = now;
        nb.cells[1].output = "done".into();
    });
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["running"], json!([b]));
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": Y })).unwrap()["running"], false);

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

    let listed = s.call("", "list_notebooks", json!({})).unwrap();
    assert_eq!((&listed[0]["running"], &listed[0]["packages"]), (&json!([X, Y]), &installing));

    let began = Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    let read = super::tools::tool_json(s.notebooks.tool("", "read_cell", &json!({ "notebook_id": NB, "cell_id": Y }), &Folder::Process, began).unwrap());
    assert!(began.elapsed() >= Duration::from_secs(44), "it waited");
    assert!(began.elapsed() < Duration::from_secs(super::tools::WAIT_SECONDS as u64), "within the cap");
    assert_eq!((&read["queued"], &read["packages"]), (&json!(true), &installing));
    let message = read["message"].as_str().unwrap();
    for part in ["Julia is getting DataFrames", "precompiling, about 2 minutes so far", "normal the first time", "call read_cell", "Precompiling DataFrames..."] {
        assert!(message.contains(part), "{part}: {message}");
    }

    let gone = s.notebooks.tool_watched("", "read_cell", &json!({ "notebook_id": NB, "cell_id": X }), &Folder::Process, Instant::now(), &|| true);
    assert!(gone.is_err_and(|e| e.contains("client_gone")));
    assert_eq!(s.refused("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "using CSV" })), "read_required");

    s.engine.with(NB, |nb| nb.packages.as_mut().unwrap()["seconds"] = json!(660));
    let began = Instant::now().checked_sub(Duration::from_secs(44)).expect("a clock that has run that long");
    let late = super::tools::tool_json(s.notebooks.tool("", "read_cell", &json!({ "notebook_id": NB, "cell_id": Y }), &Folder::Process, began).unwrap());
    let message = late["message"].as_str().unwrap();
    for part in ["about 11 minutes", "longer than usual", "Tell the user", "only if that line has changed", "stop waiting"] {
        assert!(message.contains(part), "{part}: {message}");
    }
    s.engine.with(NB, |nb| nb.packages = Some(installing.clone()));

    s.engine.with(NB, |nb| nb.cells[1].queued = false);
    s.read("", NB, Y);
    s.edit("", NB, Y, "y = 2");
    let ran = s.call("", "submit_changes", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(ran["packages"], installing, "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("DataFrames"), "{ran}");
    s.engine.with(NB, |nb| nb.cells[1].queued = true);

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

    running(true);
    let began = Instant::now();
    let at_once = read("7", began);
    assert_eq!(at_once["running"], true);
    assert!(began.elapsed() < Duration::from_secs(1), "the user's run isn't waited for");
    assert_eq!(at_once.get("message"), None, "nothing was waited for, so nothing ran out: {at_once}");

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

    for wait in [true, false] {
        let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": wait })).unwrap();
        if wait {
            assert_eq!(ran["execution"]["still_running"], json!([Y]), "{ran}");
        }
        let began = late();
        assert_eq!(read(began)["running"], true);
        assert!(began.elapsed() >= Duration::from_secs(44), "waited for the dependent (wait_for_completion {wait})");

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

    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let began = late();
    let receipt = submit(began);
    assert_eq!((&receipt["execution"]["status"], &receipt["execution"]["still_running"]), (&json!("running"), &json!([Y])), "{receipt}");
    assert!(began.elapsed() >= Duration::from_secs(44), "waited for the run");

    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-1", "cells": [X] } }));
    let began = Instant::now();
    assert_eq!(submit(began)["execution"]["status"], "completed");
    assert!(began.elapsed() < Duration::from_secs(1), "nothing to wait for");
    let receipt = super::tools::tool_json(s.notebooks.tool("8", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, Instant::now()).unwrap());
    assert_eq!(receipt["execution"]["status"], "completed", "{receipt}");
}

#[test]
fn a_waited_submit_with_nothing_staged_says_how_the_cells_that_ran_meanwhile_went() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 1"), (Y, "y = x + linger()")]);
    let submit = || super::tools::tool_json(s.notebooks.tool("7", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, Instant::now()).unwrap());
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
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let receipt = std::thread::scope(|scope| {
        scope.spawn(|| ends(false));
        submit()
    });
    assert_eq!((&receipt["execution"]["status"], &receipt["affected_cells"]), (&json!("completed"), &json!([Y])), "{receipt}");
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
    let began = Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    let receipt = super::tools::tool_json(s.notebooks.tool("7", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, began).unwrap());
    assert_eq!((&receipt["execution"]["status"], &receipt["execution"]["still_running"]), (&json!("running"), &json!([Y])), "{receipt}");
    assert!(began.elapsed() >= Duration::from_secs(44), "waited for the run");
    let receipt = super::tools::tool_json(s.notebooks.tool("8", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, Instant::now()).unwrap());
    assert_eq!(receipt["execution"]["status"], "completed", "{receipt}");
    s.engine.with(NB, |nb| nb.cells[1].running = false);
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-1", "cells": [X, Y] } }));
    let began = Instant::now();
    let receipt = super::tools::tool_json(s.notebooks.tool("7", "submit_changes", &json!({ "notebook_id": NB, "wait_for_completion": true }), &Folder::Process, began).unwrap());
    assert_eq!(receipt["execution"]["status"], "completed", "{receipt}");
    assert!(began.elapsed() < Duration::from_secs(1), "nothing to wait for");
}

#[test]
fn open_notebooks_run_of_the_notebook_is_the_sessions_run() {
    let s = setup();
    let path = temp_notebooks("open-run", 1).remove(0);
    *s.engine.opens_with.lock().unwrap() = vec![(X.into(), "x = 1".into()), (Y.into(), "y = x + linger()".into())];
    let id = s.call("7", "open_notebook", json!({ "path": path, "run_notebook": true })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    assert_eq!(s.notebooks.session_runs("7"), vec![id.clone()], "the session's Stop reaches the open's run");
    s.engine.with(&id, |nb| {
        nb.cells[0].queued = false;
        nb.cells[1].queued = false;
        nb.cells[1].running = true;
    });
    let late = || Instant::now().checked_sub(Duration::from_secs(43)).expect("a clock that has run that long");
    let tool = |owner: &str, name: &str, args: Value, began: Instant| super::tools::tool_json(s.notebooks.tool(owner, name, &args, &Folder::Process, began).unwrap());
    let began = late();
    let read = tool("7", "read_cell", json!({ "notebook_id": id, "cell_id": Y }), began);
    assert_eq!(read["running"], true, "{read}");
    assert!(read["message"].as_str().is_some_and(|m| m.contains("the cell is still running")), "the read says its wait ran out: {read}");
    assert!(began.elapsed() >= Duration::from_secs(44), "the read waited for the run");
    let began = late();
    let receipt = tool("7", "submit_changes", json!({ "notebook_id": id, "wait_for_completion": true }), began);
    assert_eq!((&receipt["execution"]["status"], &receipt["execution"]["still_running"]), (&json!("running"), &json!([Y])), "{receipt}");
    assert!(began.elapsed() >= Duration::from_secs(44), "the submit waited for the run");
    let began = Instant::now();
    assert_eq!(tool("8", "read_cell", json!({ "notebook_id": id, "cell_id": Y }), began)["running"], true);
    assert!(began.elapsed() < Duration::from_secs(1), "another session's read isn't held up");
    s.engine.with(&id, |nb| nb.cells[1].running = false);
    let began = Instant::now();
    let receipt = tool("7", "submit_changes", json!({ "notebook_id": id, "wait_for_completion": true }), began);
    assert_eq!(receipt["execution"]["status"], "completed", "{receipt}");
    assert!(began.elapsed() < Duration::from_secs(1), "nothing to wait for");
}

#[test]
fn an_open_without_its_run_starts_no_run_of_the_sessions() {
    let s = setup();
    let path = temp_notebooks("open-preview", 1).remove(0);
    *s.engine.opens_with.lock().unwrap() = vec![(X.into(), "x = 1".into())];
    let id = s.call("7", "open_notebook", json!({ "path": path })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    assert_eq!(s.notebooks.session_runs("7"), Vec::<String>::new(), "safe preview runs nothing");
    s.engine.with(&id, |nb| nb.cells[0].running = true);
    let began = Instant::now();
    super::tools::tool_json(s.notebooks.tool("7", "read_cell", &json!({ "notebook_id": id, "cell_id": X }), &Folder::Process, began).unwrap());
    assert!(began.elapsed() < Duration::from_secs(1), "a run the session didn't start isn't waited for");
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
    let s = setup();
    s.engine.open(NB, "/n/a.R", &[(X, "x = 1"), (Y, "y = x + 1"), ("33333333-3333-3333-3333-333333333333", "z = 1 # wide table"), ("44444444-4444-4444-4444-444444444444", "w = z # wide table")]);
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
    let (z, w) = ("33333333-3333-3333-3333-333333333333", "44444444-4444-4444-4444-444444444444");
    s.seconds(1.0);
    s.engine.with(NB, |nb| nb.runs_first = vec![z.into()]);
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": w, "wait_for_completion": true })).unwrap();
    let text = |cell: &str| ran["outputs"]["changed"].as_array().unwrap().iter().find(|c| c["cell_id"] == cell).unwrap_or_else(|| panic!("{ran}"))["output_text"].as_str().unwrap().to_owned();
    assert!(text(z).len() < 250 && text(w).len() == 600, "{ran}");

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

    let staged = call("A", "edit_cell", json!({ "cell_id": b, "code": "b = a + 2", "run_after": true })).unwrap();
    assert_eq!((&staged["execution"]["status"], staged["pending_run"].as_array().unwrap().contains(&json!(b))), (&json!("staged"), true));
    let warned = format!("{} The edit is staged, not run.", conflict.strip_prefix("ArgumentError: ").unwrap());
    assert!(staged["warnings"].as_array().unwrap().contains(&json!(warned)));

    s.read("A", NB, a);
    let cleared = call("A", "execute_cell", json!({ "cell_id": b, "wait_for_completion": true })).unwrap();
    assert_eq!(cleared["execution"]["status"], "completed");

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
            assert_eq!(meta, json!({ "cell_id": X, "shown_as": "text/plain", "png_bytes": 4, "interactive": false, "message": super::tools::STATIC_PICTURE }));
            assert_eq!(png_base64, "iVBORw==");
        }
        Reply::Json(_) => panic!("no image"),
    }
    assert_eq!(view(Y).err(), Some(format!("ArgumentError: no_image::Cell {Y}: its output (text/plain) has no PNG rendering; read_cell shows it as text")));
    s.engine.open("44444444-0000-4000-8000-000000000004", "/n/g.jl", &[(X, "gr plot(x)")]);
    match s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": "44444444-0000-4000-8000-000000000004", "cell_id": X }), &Folder::Process, Instant::now()).unwrap() {
        Reply::Image { meta, .. } => assert_eq!((&meta["interactive"], &meta["message"]), (&json!(false), &json!(super::tools::STATIC_PICTURE))),
        Reply::Json(_) => panic!("no image"),
    }
    s.engine.open("55555555-0000-4000-8000-000000000005", "/n/w.jl", &[(X, "wrapped plot(x)")]);
    match s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": "55555555-0000-4000-8000-000000000005", "cell_id": X }), &Folder::Process, Instant::now()).unwrap() {
        Reply::Image { meta, .. } => assert_eq!((&meta["shown_as"], &meta["interactive"], &meta["message"]), (&json!("text/html"), &json!(false), &json!(super::tools::STATIC_PICTURE))),
        Reply::Json(_) => panic!("no image"),
    }
    s.engine.with(NB, |nb| nb.cells[0].errored = true);
    assert_eq!(view(X).err(), Some(format!("ArgumentError: no_image::Cell {X} errored; read_cell shows the error")));
    s.engine.with(NB, |nb| nb.cells[1].queued = true);
    assert_eq!(view(Y).err(), Some(format!("ArgumentError: no_image::Cell {Y} is queued to run, so it has no output yet; read_cell says when it has finished")));
    s.engine.with(NB, |nb| (nb.cells[1].queued, nb.cells[1].running) = (false, true));
    assert_eq!(view(Y).err(), Some(format!("ArgumentError: no_image::Cell {Y} is running, so it has no output yet; read_cell says when it has finished")));
    s.engine.with(NB, |nb| (nb.cells[1].running, nb.cells[1].not_run) = (false, true));
    assert_eq!(view(Y).err(), Some(format!("ArgumentError: no_image::Cell {Y} hasn't run, so it has no output yet; run it first (execute_cell)")));
    s.engine.with(NB, |nb| (nb.cells[1].not_run, nb.cells[1].running, nb.cells[1].output) = (false, true, "1".into()));
    assert!(view(Y).err().unwrap().contains("has no PNG rendering"));
}

#[test]
fn view_cell_output_says_an_interactive_output_was_not_seen_working() {
    const Z: &str = "33333333-3333-3333-3333-333333333333";
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "plotly(x)"), (Y, "scripted html"), (Z, "html")]);
    let view = |cell: &str| s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": NB, "cell_id": cell }), &Folder::Process, Instant::now());
    let Reply::Image { meta, .. } = view(X).unwrap() else { panic!("no image") };
    assert_eq!(meta["interactive"], true);
    assert_eq!(meta["message"], super::tools::INTERACTIVE_PICTURE);
    let unseen = view(Y).err().unwrap();
    assert_eq!(unseen, format!("ArgumentError: no_image::Cell {Y}: its output (text/html) has no PNG rendering; read_cell shows it as text. {}", super::tools::INTERACTIVE_UNSEEN));
    let error = json!({ "isError": true, "content": [{ "type": "text", "text": json!({ "error": "no_image", "message": unseen.split_once("no_image::").unwrap().1 }).to_string() }] });
    assert!(super::tools::snapshot_may_help(&error), "the app's picture can still stand in");
    assert_eq!(view(Z).err(), Some(format!("ArgumentError: no_image::Cell {Z}: its output (text/html) has no PNG rendering; read_cell shows it as text")));
    let shot = super::tools::screenshot_result(Y, "iVBORw==".to_owned().into(), &error).unwrap();
    let said: Value = serde_json::from_str(shot["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((said["interactive"].clone(), said["message"].clone(), said["shown_as"].clone()), (json!(true), json!(super::tools::INTERACTIVE_PICTURE), json!("text/html")));
    assert_eq!(said["cut"], Value::Null, "a whole output says nothing of the kind");
    let cut = super::tools::screenshot_result(Y, crate::Picture { png: "iVBORw==".into(), cut: Some((855, 920)) }, &error).unwrap();
    let said: Value = serde_json::from_str(cut["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(said["cut"], "The picture shows 855 px of an output 920 px tall; the rest is missing.");
}

#[test]
fn view_cell_output_says_when_the_picture_is_from_an_earlier_run() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "plot(x)")]);
    s.engine.with(NB, |nb| nb.cells[0].output = "plot".into());
    let view = |s: &Setup| match s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": NB, "cell_id": X }), &Folder::Process, Instant::now()).unwrap() {
        Reply::Image { meta, .. } => meta,
        Reply::Json(_) => panic!("no image"),
    };
    assert_eq!(view(&s).get("state"), None, "a current picture says nothing more");
    for (set, state, says) in [
        ((true, false, false), "running", "may be from its previous run"),
        ((false, true, false), "queued", "queued to run again"),
        ((false, false, true), "stale", "a cell it depends on has changed"),
    ] {
        s.engine.with(NB, |nb| (nb.cells[0].running, nb.cells[0].queued, nb.cells[0].stale) = set);
        let meta = view(&s);
        assert_eq!(meta["state"], state, "{meta}");
        assert!(meta["message"].as_str().unwrap().contains(says), "{meta}");
        assert_eq!(meta["png_bytes"], 4, "the picture is still sent: {meta}");
    }
    s.engine.with(NB, |nb| nb.cells[0].stale = false);
    s.read("", NB, X);
    s.edit("", NB, X, "plot(x, y)");
    let meta = view(&s);
    assert_eq!(meta["state"], "stale", "{meta}");
    assert!(meta["message"].as_str().unwrap().contains("before its code was edited"), "{meta}");
}

#[test]
fn view_cell_output_says_when_the_picture_came_from_the_html() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "widget(fig)")]);
    match s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": NB, "cell_id": X }), &Folder::Process, Instant::now()).unwrap() {
        Reply::Image { meta, .. } => assert_eq!(meta, json!({ "cell_id": X, "shown_as": "text/html", "png_bytes": 4, "source": "embedded image", "interactive": false, "message": super::tools::STATIC_PICTURE })),
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
    let busy = crate::mcp::tool_error(&format!("no_image::Cell {X}: the notebook's worker is busy running cells and didn't render its value within 10 seconds; view it again when read_cell says they have finished"), false);
    assert!(super::snapshot_may_help(&busy), "a worker running cells");
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
fn read_cell_cuts_a_huge_text_output_to_its_start_and_end() {
    let s = setup();
    let long = "1".repeat(20_000);
    s.engine.open(NB, "/n/a.jl", &[(X, "Text(join(string.(1:200_000), '\\n'))"), (Y, &long)]);
    let big = format!("{}€{}€{}", "a".repeat(11_999), "b".repeat(1_300_000), "c".repeat(3_998));
    s.engine.with(NB, |nb| nb.cells[0].output = big.clone());
    let read = s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let output = read["output"].as_str().unwrap();
    assert!(output.len() < 16_200, "{}", output.len());
    assert!(output.starts_with(&"a".repeat(11_999)) && output.ends_with(&format!("\n{}", "c".repeat(3_998))), "{output:.40}");
    assert!(output.contains(&format!("\n… ({} of this output's {} bytes are left out here. No tool shows them: to see a part, run a cell that picks it out, such as its first or last lines.)\n", big.len() - 11_999 - 3_998, big.len())), "{}", &output[11_990..12_200]);
    let ran = s.call("", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true })).unwrap();
    let summary = ran["outputs"]["changed"][0]["output_summary"].as_str().unwrap_or_else(|| panic!("{ran:.300}"));
    assert!(summary.len() < 2_200 && summary.contains("bytes are left out here. read_cell shows more.)"), "a run's reply cuts it shorter: {summary}");
    s.engine.with(NB, |nb| nb.cells[0].output = "1".repeat(16_000));
    assert_eq!(s.call("", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap()["output"], json!("1".repeat(16_000)), "16,000 bytes is whole");
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
    let late = Instant::now() - Duration::from_secs(60);
    let make = |began| s.notebooks.tool("a", "new_notebook", &json!({ "path": path }), &Folder::Process, began).map(tool_json);
    let started = Instant::now();
    let said = make(late).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(said.starts_with(&format!("notebook_opening::Creating '{path}' is taking longer than one call waits")), "{said}");
    assert_eq!(crate::mcp::tool_error(&said, false)["isError"], false, "still opening is not a failure");

    release.send(()).unwrap();
    let made = make(Instant::now()).unwrap();
    assert_eq!((&made["path"], &made["created"]), (&json!(path), &json!(true)), "{made}");
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|m| *m == "new").count(), 1, "made once");
    assert!(make(Instant::now()).unwrap_err().contains("file_exists::"));
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|m| *m == "new").count(), 1);

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
fn a_notebook_made_after_the_call_stopped_waiting_is_still_the_sessions() {
    let s = setup();
    let dir = temp_notebooks("late-made", 1)[0].rsplit_once(SEP).unwrap().0.to_owned();
    let late = Instant::now() - Duration::from_secs(60);
    let refusal = |result: Result<Value, String>| result.err().filter(|e| e.starts_with("ArgumentError: one_notebook::"));
    let made_late = |owner: &str, name: &str| {
        let path = format!("{dir}{SEP}{name}.jl");
        let (release, held) = mpsc::channel();
        *s.engine.hold_open.lock().unwrap() = Some(held);
        let said = s.notebooks.tool(owner, "new_notebook", &json!({ "path": path }), &Folder::Process, late).map(tool_json).unwrap_err();
        assert!(said.starts_with("notebook_opening::"), "{said}");
        release.send(()).unwrap();
        let began = Instant::now();
        while s.notebooks.left.lock().unwrap().iter().any(|(_, opening)| opening.done.lock().unwrap().is_none()) {
            assert!(began.elapsed() < Duration::from_secs(10), "the creation didn't finish");
            std::thread::sleep(Duration::from_millis(10));
        }
        let listed = s.call(owner, "list_notebooks", json!({})).unwrap();
        let id = listed.as_array().unwrap().iter().find(|nb| nb["path"] == json!(path)).unwrap_or_else(|| panic!("{listed}"))["notebook_id"].as_str().unwrap().to_owned();
        (path, id)
    };

    s.notebooks.allow_several("d");
    let (_, id) = made_late("d", "several");
    assert_eq!(refusal(s.call("d", "run_all_cells", json!({ "notebook_id": id }))), None);
    s.notebooks.allow_several("other");
    assert!(refusal(s.call("other", "run_all_cells", json!({ "notebook_id": id }))).is_some());

    let (path, _) = made_late("f", "one");
    assert_eq!(s.notebooks.bound("f"), Some(canonical_path(&path).unwrap()));

    let path = format!("{dir}{SEP}nb0.jl");
    let (release, held) = mpsc::channel();
    *s.engine.hold_open.lock().unwrap() = Some(held);
    let open = |owner: &str, began| s.notebooks.tool(owner, "open_notebook", &json!({ "path": path }), &Folder::Process, began).map(tool_json);
    s.notebooks.allow_several("g");
    s.notebooks.allow_several("h");
    assert!(open("g", late).unwrap_err().starts_with("notebook_opening::"));
    assert!(open("h", late).unwrap_err().starts_with("notebook_opening::"));
    release.send(()).unwrap();
    let id = open("h", Instant::now()).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    assert_eq!(refusal(s.call("g", "run_all_cells", json!({ "notebook_id": id }))), None);
    assert_eq!(refusal(s.call("h", "run_all_cells", json!({ "notebook_id": id }))), None);
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
    let started = Instant::now();
    assert!(allow(late).unwrap_err().starts_with("notebook_opening::"));
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());

    release.send(()).unwrap();
    let allowed = allow(Instant::now()).unwrap();
    assert_eq!((&allowed["already_allowed"], &allowed["ran"], &allowed["execution_allowed"]), (&json!(false), &json!(true), &json!(true)), "{allowed}");
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|m| *m == "allow_execution").count(), 1, "allowed once");
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
    let code = s.call("", "read_notebook_code", json!({ "notebook_id": previewed["notebook_id"] })).unwrap();
    assert_eq!(code["note"], json!(super::tools::NOT_RUN), "{code}");
    let code = s.call("", "read_notebook_code", json!({ "notebook_id": opened["notebook_id"] })).unwrap();
    assert_eq!(code.get("note"), None, "{code}");
    let lazy = format!("{dir}{SEP}lazy.jl");
    std::fs::copy(format!("{dir}{SEP}nb1.jl"), &lazy).unwrap();
    assert_eq!(s.call("", "open_notebook", json!({ "path": lazy })).unwrap()["on_cell_change"], "lazy");
    let opens = || s.engine.calls.lock().unwrap().iter().filter(|call| *call == "open").count();
    let before = opens();
    for (name, text) in [("fit.jl", "using Statistics\nmean([1, 2])\n"), ("fit.R", "x <- mean(c(1, 2))\n"), ("empty.jl", ""), ("notes.txt", "### A Pluto.jl notebook ###\n")] {
        let script = format!("{dir}{SEP}{name}");
        std::fs::write(&script, text).unwrap();
        assert_eq!(s.call("", "open_notebook", json!({ "path": script })), Err(format!("ArgumentError: not_a_notebook::'{script}' isn't a notebook: it doesn't start with a Pluto or Ember notebook's first line, and it isn't a marimo app")));
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
        assert_eq!(opened, Err("unsupported::R notebooks aren't running in this runtime".into()));
    }

    let made = s.call("", "new_notebook", json!({ "path": format!("{dir}{SEP}.{SEP}fresh.jl") })).unwrap();
    assert_eq!((&made["path"], &made["created"], &made["ran"]), (&json!(format!("{dir}{SEP}fresh.jl")), &json!(true), &json!(true)));
    s.edit("", made["notebook_id"].as_str().unwrap(), made["cell_ids"][0].as_str().unwrap(), "x = 1");
    assert_eq!(s.call("", "new_notebook", json!({ "path": path })), Err(format!("ArgumentError: file_exists::'{path}' already exists; use open_notebook to load it")));
    assert_eq!(s.refused("", "new_notebook", json!({ "path": format!("{dir}{SEP}x.txt") })), "invalid_path");
    assert_eq!(s.call("", "new_notebook", json!({ "path": format!("{dir}{SEP}missing{SEP}y.jl") })), Err(format!("ArgumentError: invalid_path::Directory does not exist: '{dir}{SEP}missing'")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = format!("{dir}{SEP}locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(format!("{locked}{SEP}probe"), "").is_err() {
            let refused = s.call("", "new_notebook", json!({ "path": format!("{locked}{SEP}z.jl") })).unwrap_err();
            assert!(refused.starts_with(&format!("ArgumentError: folder_not_writable::A new notebook can't be saved in the folder, '{locked}': it is read-only.")) && refused.contains("`path`"), "{refused}");
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
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
        Err(format!("KeyError: key \"notebook_not_found::No notebook with id '123' in the current session. Open now: '{NB}' (/n/a.jl).\" not found"))
    );
    assert_eq!(read(json!({ "notebook_id": NB, "cell_id": "zzz" })), Err("ArgumentError: invalid_cell_id::Invalid cell ID: 'zzz'".into()));
    assert_eq!(read(json!({ "notebook_id": NB, "cell_id": 7 })), not_found("cell", "7", "notebook"));
    assert_eq!(read(json!({ "notebook_id": NB })), Err("KeyError: key \"cell_id\" not found".into()));
    let cell = read(json!({ "notebook_id": NB.to_uppercase(), "cell_id": X })).unwrap();
    assert_eq!(cell, json!({ "cell_id": X, "code": "x = 1", "output": "", "errored": false, "running": false, "queued": false, "code_folded": false, "stale": false }));
    assert_eq!(s.call("", "no_such_tool", json!({})), Err("ArgumentError: unknown_tool::Unknown tool: 'no_such_tool'".into()));
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
    let roundabout = format!("{first_nb}{SEP}..{SEP}{}", first_nb.rsplit(SEP).next().unwrap());
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": roundabout }), &Folder::Process), None);
    if let Some(relative) = pathdiff(first_nb, &wire::files::real_path(&std::env::current_dir().unwrap()).unwrap().display().to_string()) {
        assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": relative }), &Folder::Process), None);
    }
    #[cfg(unix)]
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": "~bob/x.jl" }), &Folder::Process), Some("ArgumentError: ~user tilde expansion not yet implemented".into()));

    let (folder, name) = first_nb.rsplit_once(SEP).unwrap();
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": name }), &Folder::In(folder.to_owned())), None);
    let other = second_nb.rsplit_once(SEP).unwrap().1;
    assert!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": other }), &Folder::In(folder.to_owned())).unwrap().contains("one_notebook"));

    let second_id = s.call("", "open_notebook", json!({ "path": second_nb })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    s.notebooks.publish();

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

    s.notebooks.bind("b", second_nb);
    assert_eq!(refused(s.call("b", "open_notebook", json!({ "path": first_nb }))), one);
    assert_eq!(refused(s.call("b", "run_all_cells", json!({ "notebook_id": first_id }))), one);
    assert_eq!(refused(s.call("b", "get_cell_order", json!({ "notebook_id": first_id }))), None);
    s.notebooks.bind("b", "");
    assert_eq!(s.notebooks.bound("b"), None);
    assert_eq!(refused(s.call("b", "run_all_cells", json!({ "notebook_id": first_id }))), None);

    std::fs::remove_file(&paths[2]).unwrap();
    s.call("c", "new_notebook", json!({ "path": &paths[2] })).unwrap();
    assert_eq!(s.notebooks.bound("c").as_ref(), Some(&paths[2]));
    assert_eq!(refused(s.call("c", "new_notebook", json!({ "path": &paths[3] }))), one);
    assert_eq!(refused(s.call("c", "open_notebook", json!({ "path": first_nb }))), one);
}

#[test]
fn a_session_that_may_work_on_several_notebooks() {
    let s = setup();
    let paths = temp_notebooks("several", 4);
    let refusal = |result: Result<Value, String>| result.err().filter(|e| e.starts_with("ArgumentError: one_notebook::"));
    s.notebooks.allow_several("d");
    let first = s.call("d", "open_notebook", json!({ "path": &paths[0] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    let second = s.call("d", "open_notebook", json!({ "path": &paths[1] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    std::fs::remove_file(&paths[2]).unwrap();
    let third = s.call("d", "new_notebook", json!({ "path": &paths[2] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    for id in [&first, &second, &third] {
        assert_eq!(refusal(s.call("d", "run_all_cells", json!({ "notebook_id": id }))), None);
    }
    let apps = s.call("", "open_notebook", json!({ "path": &paths[3] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    s.notebooks.publish();
    assert_eq!(
        refusal(s.call("d", "run_all_cells", json!({ "notebook_id": apps }))),
        Some(format!(
            "ArgumentError: one_notebook::{} wasn't opened or created here, so you can't change or run it. \
             If the user wants you to work on it, open it with `open_notebook` and its path first.",
            paths[3]
        ))
    );
    assert_eq!(refusal(s.call("d", "read_notebook_code", json!({ "notebook_id": apps }))), None);
    let listed = s.call("d", "list_notebooks", json!({})).unwrap();
    assert!(listed.as_array().unwrap().iter().all(|nb| nb["this_session"] == false), "{listed}");

    s.notebooks.stop_notebook(&paths[0]).unwrap();
    s.call("d", "list_notebooks", json!({})).unwrap();
    assert!(refusal(s.call("d", "run_all_cells", json!({ "notebook_id": apps }))).is_some());
    assert_eq!(refusal(s.call("d", "run_all_cells", json!({ "notebook_id": second }))), None);
    assert_eq!(s.call("d", "open_notebook", json!({ "path": &paths[3] })).unwrap()["already_open"], true);
    assert_eq!(refusal(s.call("d", "run_all_cells", json!({ "notebook_id": apps }))), None);

    s.call("e", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    assert!(refusal(s.call("e", "open_notebook", json!({ "path": &paths[2] }))).is_some());
}

#[test]
fn close_notebook_ends_its_own_notebook_and_no_other() {
    let s = setup();
    let paths = temp_notebooks("close", 2);
    let mine = s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    let theirs = s.call("b", "open_notebook", json!({ "path": &paths[1] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    s.notebooks.publish();
    assert!(s.call("a", "close_notebook", json!({ "notebook_id": theirs })).unwrap_err().starts_with("ArgumentError: one_notebook::"));
    let closed = s.call("a", "close_notebook", json!({ "notebook_id": mine })).unwrap();
    assert_eq!(closed, json!({ "notebook_id": mine, "path": paths[0], "closed": true }));
    let open: Vec<String> = s.notebooks.snapshots().unwrap().into_iter().map(|nb| nb.path).collect();
    assert_eq!(open, [paths[1].clone()]);
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[0]));
    let other = format!("{}-other.jl", paths[0].trim_end_matches(".jl"));
    for (tool, args) in [("new_notebook", json!({})), ("new_notebook", json!({ "path": &other })), ("open_notebook", json!({ "path": &paths[1] }))] {
        let refused = s.call("a", tool, args).unwrap_err();
        assert!(refused.starts_with("ArgumentError: one_notebook::") && refused.contains("You closed it") && refused.contains("If the user didn't ask you to close it, open it again now with open_notebook and its path"), "{tool}: {refused}");
    }
    s.engine.with(&theirs, |nb| nb.broken = true);
    assert!(s.call("a", "new_notebook", json!({})).unwrap_err().starts_with("ArgumentError: one_notebook::"));
    s.engine.with(&theirs, |nb| nb.broken = false);
    assert!(s.call("a", "new_notebook", json!({})).unwrap_err().contains("You closed it"));
    assert!(s.call("a", "open_notebook", json!({ "path": &paths[0] })).is_ok());
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[0]));
    s.notebooks.stop_notebook(&paths[0]).unwrap();
    assert!(s.call("a", "open_notebook", json!({ "path": &paths[1] })).is_ok());
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[1]));
}

#[test]
fn a_session_whose_closed_notebook_the_user_opens_again_is_freed_when_the_user_closes_it() {
    for seen_by in ["add_cell", "list_notebooks"] {
        let s = setup();
        let paths = temp_notebooks(&format!("reopen-{seen_by}"), 2);
        let mine = s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
        s.call("a", "close_notebook", json!({ "notebook_id": mine })).unwrap();
        let again = s.call("", "open_notebook", json!({ "path": &paths[0] })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
        if seen_by == "add_cell" {
            s.call("a", "add_cell", json!({ "notebook_id": again, "code": "z = 3" })).unwrap();
        } else {
            s.call("a", "list_notebooks", json!({})).unwrap();
        }
        s.notebooks.stop_notebook(&paths[0]).unwrap();
        let other = s.call("a", "open_notebook", json!({ "path": &paths[1] }));
        assert!(other.is_ok(), "{seen_by}: {other:?}");
        assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[1]), "{seen_by}");
    }
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

    s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    assert!(s.call("c", "open_notebook", json!({ "path": &paths[0] })).unwrap_err().starts_with("ArgumentError: one_notebook::"));
    assert_eq!(s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap()["already_open"], true);
    assert_eq!(s.notebooks.bound("c").as_ref(), Some(&paths[1]));

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
    s.notebooks.bind("a", &paths[0]);
    s.notebooks.note_call("d", false, None);
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[0]));
    assert!(s.notebooks.state.lock().unwrap().seen.contains_key("a"));
    assert_eq!(s.notebooks.bound("c"), None);
    assert!(!s.notebooks.state.lock().unwrap().bindings.contains_key("c"));

    s.seconds(8.0 * 24.0 * 3600.0);
    s.call("a", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.notebooks.note_call("e", false, None);
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
    s.notebooks.note_call("b", false, None);
    s.seconds(125.7);
    assert_eq!(recent("a", 900.0), json!({ "count": 1, "active_seconds_ago": 125 }));
    assert_eq!(recent("b", 900.0)["count"], 0, "a session is not another to itself");

    s.notebooks.note_call("c", false, None);
    s.seconds(10.0);
    assert_eq!(recent("a", 900.0), json!({ "count": 2, "active_seconds_ago": 10 }));
    assert_eq!(recent("a", 60.0), json!({ "count": 1, "active_seconds_ago": 10 }), "b called longer ago than that");
    assert_eq!(recent("", 900.0)["count"], 2, "the app's calls have no session, so every session is another");

    s.notebooks.bind("app", &paths[0]);
    s.notebooks.note_call("unbound", false, None);
    assert_eq!(recent("a", 900.0)["count"], 2);
    s.engine.notebooks.lock().unwrap().retain(|nb| nb.path != paths[1]);
    assert_eq!(recent("a", 900.0), json!({ "count": 1, "active_seconds_ago": 135 }), "c's notebook is closed, and b called 135 s ago");
    assert!(s.notebooks.state.lock().unwrap().seen.contains_key("unbound"));
    s.call("c", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    s.notebooks.note_call("c", false, None);

    s.notebooks.bind("b", "");
    assert!(s.notebooks.state.lock().unwrap().seen.contains_key("b"));
    assert_eq!(recent("a", 900.0)["count"], 1, "b works in no notebook now");
    s.notebooks.note_call("", false, None);
    assert!(!s.notebooks.state.lock().unwrap().seen.contains_key(""), "a call with no session isn't recorded");
    assert_eq!(s.notebooks.state.lock().unwrap().seen.len(), 5, "a, b, c, unbound and the app's bound session");

    s.seconds(6.0 * 24.0 * 3600.0);
    s.notebooks.note_call("a", false, None);
    s.seconds(2.0 * 24.0 * 3600.0);
    assert_eq!(recent("c", 1.0e9), json!({ "count": 1, "active_seconds_ago": 172800 }));
    assert_eq!(s.notebooks.state.lock().unwrap().seen.len(), 1, "c and the app's session went 8 days ago, a 2");
}

#[test]
fn the_plugin_is_in_the_runtime_while_its_front_runs_and_a_notebook_it_works_in_is_open() {
    let s = setup();
    let paths = temp_notebooks("shared", 2);
    let front = |pid| crate::mcp::Front { pid, started: Some(1), boot: None };
    let running = std::sync::Mutex::new(vec![7]);
    let alive = |f: &crate::mcp::Front| running.lock().unwrap().contains(&f.pid);
    assert!(!s.notebooks.shared(&alive), "no session at all");

    s.call("app", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.notebooks.note_call("app", true, None);
    assert!(!s.notebooks.shared(&alive));

    s.call("p", "open_notebook", json!({ "path": &paths[1] })).unwrap();
    assert!(!s.notebooks.shared(&alive), "bound, but it has made no call");
    s.notebooks.note_call("p", false, Some(&front(7)));
    assert!(s.notebooks.shared(&alive));
    s.seconds(3.0 * 24.0 * 3600.0);
    assert!(s.notebooks.shared(&alive), "a running front counts however long ago it called");
    running.lock().unwrap().clear();
    assert!(!s.notebooks.shared(&alive), "its front has gone");
    running.lock().unwrap().push(7);
    s.engine.notebooks.lock().unwrap().retain(|nb| nb.path != paths[1]);
    assert!(!s.notebooks.shared(&alive), "its notebook closed");

    s.call("q", "open_notebook", json!({ "path": &paths[0] })).unwrap();
    s.notebooks.note_call("q", false, None);
    s.seconds(29.0 * 60.0);
    assert!(s.notebooks.shared(&alive));
    s.seconds(2.0 * 60.0);
    assert!(!s.notebooks.shared(&alive), "it called over half an hour ago");
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

    close(&paths[0]);
    assert!(s.call("a", "open_notebook", json!({ "path": &paths[2] })).is_ok());
    assert_eq!(s.notebooks.bound("a").as_ref(), Some(&paths[2]));
    close(&paths[2]);
    assert_eq!(mine("a"), 0);
    assert_eq!(s.notebooks.bound("a"), None);
    assert!(s.call("a", "new_notebook", json!({})).is_ok());

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

    s.read("", &id(1), Y);
    s.edit("", &id(1), Y, "y = x + 1");
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([Y]));
    s.seconds(1.0);
    assert_eq!(s.notebooks.restart(&id(1), false), Ok(json!({ "restarted": true })));
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([]));
    assert_eq!(s.notebooks.restart(&id(2), false), Err("ArgumentError: execution_blocked::The notebook is in safe preview; Run notebook starts it".into()));
    assert_eq!(s.notebooks.restart("nope", false), Err("KeyError: key \"notebook_not_found::No notebook with id 'nope' in the current session\" not found".into()));

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

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&paths[1]).unwrap();
        let modified = meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9;
        assert_eq!(file_info(&paths[1]), Ok(json!({ "exists": true, "modified": modified })));
    }
    assert_eq!(file_info(&paths[0]), Ok(json!({ "exists": false })));
    assert_eq!(file_info(&dir), Ok(json!({ "exists": false })), "a folder isn't a file");

    s.notebooks.bind("b", &renamed);
    let made = s.notebooks.new_for("b", &Folder::In(dir.clone())).unwrap();
    assert_eq!(made["path"], format!("{dir}{SEP}made.jl"));
    assert_eq!(s.notebooks.bound("b"), Some(format!("{dir}{SEP}made.jl")));
}

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

    s.engine.with(&id(1), |nb| nb.cells[0].running = false);
    s.hours(46.0);
    assert_eq!(s.notebooks.stop_idle(), [used.clone()]);
    s.hours(2.0);
    assert_eq!(s.notebooks.stop_idle(), [running.clone()]);
    assert_eq!(open(), [kept.clone()]);

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

    s.notebooks.notified(&json!({ "method": "cell_state", "params": { "notebook_id": NB, "cells": [] } }));
    s.notebooks.publish();
    assert_eq!(held(), Some((1, 1, true, true, 1, 1, 1)));

    s.engine.notebooks.lock().unwrap().clear();
    s.notebooks.notified(&json!({ "method": "notebook_shut_down", "params": { "notebook_id": NB } }));
    assert_eq!(held(), None);

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
        |shown: &str| Err(format!("KeyError: key \"notebook_not_found::No notebook with id '{shown}' in the current session. Open now: '{NB}' (/n/a.jl).\" not found"));
    assert_eq!(keep(json!(missing), json!(true)), not_found(missing));
    assert_eq!(keep(json!(123), json!(true)), not_found("123"));
    assert_eq!(s.notebooks.keep_alive(&json!({ "keep": true })), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: ''".into()));
    assert_eq!(keep(json!(NB), json!("yes")), Err("ArgumentError: invalid_keep::keep must be true or false".into()));
    assert_eq!(keep(json!(NB.to_uppercase()), json!(false)), Ok(json!({ "notebook_id": NB, "kept_alive": false })));
    let text = |raw: &str| crate::mcp::tool_error(raw, false)["content"][0]["text"].as_str().unwrap().to_owned();
    assert_eq!(
        text(&keep(json!(missing), json!(true)).unwrap_err()),
        format!(r#"{{"error":"notebook_not_found","message":"No notebook with id '{missing}' in the current session. Open now: '{NB}' (/n/a.jl)."}}"#)
    );
}

#[test]
fn a_notebook_id_that_isnt_open_is_told_which_are() {
    let s = setup();
    let read = |nb: &str| s.call("", "read_cell", json!({ "notebook_id": nb, "cell_id": X })).unwrap_err();
    let missing = "cccccccc-0000-0000-0000-000000000000";
    assert!(read(missing).contains("No notebook is open; open one with open_notebook."), "{}", read(missing));
    let ids: Vec<String> = (0..7).map(|n| format!("aaaaaaaa-0000-0000-0000-00000000000{n}")).collect();
    for (n, id) in ids.iter().enumerate() {
        s.engine.open(id, &format!("/n/{n}.jl"), &[]);
    }
    let error = read(missing);
    assert!(error.contains("Open now: '") && ids.iter().filter(|id| error.contains(id.as_str())).count() == 5, "{error}");
    assert_eq!(error.matches(" (/n/").count(), 5, "a few at most: {error}");
    assert!(error.contains("and 2 more (list_notebooks)."), "{error}");
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

    s.engine.with(NB, |nb| nb.exited = None);
    s.notebooks.publish();
    assert_eq!(next(&rx)["notebooks"][0].get("exited"), None);

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
    let dir = std::env::temp_dir().to_string_lossy().trim_end_matches(SEP).to_owned();
    for name in ["c.jl", "c.txt"] {
        assert_eq!(s.notebooks.move_notebook(R, &format!("{dir}{SEP}{name}")), Err(format!("ArgumentError: invalid_path::Notebook path must end in .R: '{dir}{SEP}{name}'")));
    }
    assert_eq!(s.notebooks.move_notebook(NB, &format!("{dir}{SEP}c.R")), Err(format!("ArgumentError: invalid_path::Notebook path must end in .jl: '{dir}{SEP}c.R'")), "and a Julia one a Julia one");
}

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
        std::thread::sleep(Duration::from_millis(300));
        release.send(()).unwrap();
        publishing.join().unwrap();
        let made = making.join().unwrap();
        let (id, cell) = (made["notebook_id"].as_str().unwrap(), made["cell_ids"][0].as_str().unwrap());
        assert!(s.call("", "edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "x = 1" })).is_ok(), "no read needed");
    });
}

#[test]
fn an_open_during_a_summary_keeps_its_run() {
    let s = setup();
    let path = temp_notebooks("open-during-summary", 1).remove(0);
    *s.engine.opens_with.lock().unwrap() = vec![(X.into(), "x = 1".into())];
    let (read_tx, read) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *s.engine.hold_snapshot.lock().unwrap() = Some((read_tx, release_rx));
    std::thread::scope(|scope| {
        let publishing = scope.spawn(|| s.notebooks.publish());
        read.recv().unwrap();
        let id = s.call("7", "open_notebook", json!({ "path": path, "run_notebook": true })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
        release.send(()).unwrap();
        publishing.join().unwrap();
        assert_eq!(s.notebooks.session_runs("7"), vec![id], "the open's run is still the session's");
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
    assert!(!message.contains("built on this computer"), "{message}");
    assert_eq!(read["packages"]["packages"], json!(["dplyr"]));
    ember.with(R, |nb| nb.packages.as_mut().unwrap()["source_builds"] = json!(["cli 3.6.2", "rlang 1.1.4"]));
    let read = super::tools::tool_json(s.notebooks.tool("", "read_cell", &json!({ "notebook_id": R, "cell_id": Y }), &Folder::Process, Instant::now()).unwrap());
    let message = read["message"].as_str().unwrap();
    assert!(message.contains("These have no binary for this R at the notebook's package date, so they are being built on this computer: cli 3.6.2, rlang 1.1.4."), "{message}");
    assert!(message.contains("A newer package date has binaries (preview_package_update)."), "{message}");
}

#[test]
fn an_r_install_waiting_for_an_ok_says_to_ask_the_user_and_nothing_waits_for_it() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "library(BSgenome.Hsapiens.UCSC.hg38)"), (Y, "genome <- BSgenome.Hsapiens.UCSC.hg38")]);
    let asking = json!({ "step": "asking", "packages": ["BSgenome.Hsapiens.UCSC.hg38"], "seconds": 3, "last_line": null, "download": { "bytes": 1_270_000_000u64, "packages": [
        { "package": "BSgenome.Hsapiens.UCSC.hg38", "version": "1.4.5", "bytes": 870_000_000u64 },
        { "package": "GO.db", "version": "3.22.0", "bytes": 400_000_000u64 },
        { "package": "org.Hs.eg.db", "version": "3.22.0", "bytes": null },
    ] } });
    ember.with(R, |nb| {
        nb.packages = Some(asking.clone());
        nb.cells.iter_mut().for_each(|c| c.queued = true);
    });
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("9", "list_notebooks", json!({})).unwrap();

    let began = Instant::now();
    let read = s.call("9", "read_cell", json!({ "notebook_id": R, "cell_id": Y })).unwrap();
    assert!(began.elapsed() < Duration::from_secs(1), "it didn't wait");
    assert_eq!((&read["queued"], &read["packages"]), (&json!(true), &asking));
    let message = read["message"].as_str().unwrap();
    for part in ["BSgenome.Hsapiens.UCSC.hg38 1.4.5 (870 MB), GO.db 3.22.0 (400 MB), org.Hs.eg.db 3.22.0 (size unknown), 1.3 GB in all", "ask the user whether to install it, then end your reply", "call it only once they have answered yes", "Don't call read_cell to wait"] {
        assert!(message.contains(part), "{part}: {message}");
    }

    ember.with(R, |nb| nb.cells[1].queued = false);
    s.read("9", R, Y);
    s.edit("9", R, Y, "genome <- BSgenome.Hsapiens.UCSC.hg38::BSgenome.Hsapiens.UCSC.hg38");
    let began = Instant::now();
    let ran = s.call("9", "submit_changes", json!({ "notebook_id": R, "wait_for_completion": true })).unwrap();
    assert!(began.elapsed() < Duration::from_secs(2), "it didn't wait: {ran}");
    assert_eq!((&ran["execution"]["still_running"], &ran["packages"]), (&json!([Y]), &asking), "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("call it only once they have answered yes"), "{ran}");
    let began = Instant::now();
    let again = s.call("9", "submit_changes", json!({ "notebook_id": R, "wait_for_completion": true })).unwrap();
    assert!(began.elapsed() < Duration::from_secs(2), "it didn't wait: {again}");
    assert!(again["execution"]["still_running"].as_array().unwrap().contains(&json!(Y)), "{again}");
    assert!(again["message"].as_str().unwrap().contains("call it only once they have answered yes"), "{again}");
    assert_eq!((&again["execution"]["status"], &again["warnings"]), (&json!("running"), &json!([])), "it didn't run out of time: {again}");

    ember.with(R, |nb| nb.cells[0].running = true);
    let engine = ember.clone();
    let now = *s.clock.lock().unwrap();
    let finish = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        engine.with(R, |nb| {
            nb.cells[0].running = false;
            nb.cells[1].queued = false;
            nb.cells[1].last_run = now;
            nb.cells[1].output = "done".into();
        });
    });
    let began = Instant::now();
    let read = s.call("9", "read_cell", json!({ "notebook_id": R, "cell_id": Y })).unwrap();
    finish.join().unwrap();
    assert!(began.elapsed() >= Duration::from_millis(500), "it waited while a cell ran: {read}");
    assert_eq!((&read["queued"], &read["output"]), (&json!(false), &json!("done")), "{read}");

    let approved = s.call("9", "approve_install", json!({ "notebook_id": R })).unwrap();
    assert!(approved["approved"] == true && approved["message"].as_str().unwrap().contains("installs now"), "{approved}");
    assert_eq!(ember.with(R, |nb| nb.packages.clone().unwrap()["step"].clone()), "installing");
    let refused = s.call("9", "approve_install", json!({ "notebook_id": R })).unwrap_err();
    assert!(refused.contains("nothing_to_approve"), "{refused}");

    s.engine.open(NB, "/n/a.jl", &[(Y, "y = 1")]);
    let julia = s.call("7", "approve_install", json!({ "notebook_id": NB })).unwrap_err();
    assert!(julia.contains("nothing_to_approve"), "{julia}");
}

#[test]
fn a_julia_run_that_adds_a_package_asks_first_except_in_auto_mode() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "using Example"), (Y, "y = 1")]);
    let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let message = ran["message"].as_str().unwrap();
    assert!(message.contains("Julia has to install packages this notebook doesn't have yet: Example") && message.contains(super::tools::ASK_STEP), "{ran}");
    assert!(!message.contains("download"), "a new package isn't a download: {message}");
    assert_eq!(ran["packages"]["step"], "asking", "{ran}");
    let listed = s.call("7", "list_notebooks", json!({})).unwrap();
    let listed = listed.as_array().unwrap().iter().find(|nb| nb["notebook_id"] == NB).cloned().unwrap_or_else(|| panic!("{listed}"));
    assert_eq!((&listed["running"], &listed["packages"]["step"]), (&json!([]), &json!("asking")), "{listed}");
    let approved = s.call("7", "approve_install", json!({ "notebook_id": NB })).unwrap();
    assert!(approved["approved"] == true && approved["message"].as_str().unwrap().contains("packages are approved"), "{approved}");

    s.notebooks.set_auto("8", true);
    s.engine.open("22222222-0000-4000-8000-000000000002", "/n/b.jl", &[(X, "using Example")]);
    let ran = s.call("8", "execute_cell", json!({ "notebook_id": "22222222-0000-4000-8000-000000000002", "cell_id": X })).unwrap();
    assert!(ran.get("packages").is_none() && !ran.to_string().contains("asking"), "{ran}");
    s.notebooks.set_auto("8", false);
    assert!(s.notebooks.asks_new_packages("8"));

    s.engine.open("33333333-0000-4000-8000-000000000003", "/n/c.jl", &[(X, "using Example")]);
    s.engine.with("33333333-0000-4000-8000-000000000003", |nb| nb.safe_preview = true);
    let allowed = s.call("7", "allow_execution", json!({ "notebook_id": "33333333-0000-4000-8000-000000000003" })).unwrap();
    assert!(allowed["message"].as_str().unwrap().contains("Julia has to install packages this notebook doesn't have yet: Example"), "{allowed}");
    assert_eq!((&allowed["ran"], &allowed["packages"]["step"]), (&json!(true), &json!("asking")), "{allowed}");
    assert!(allowed.get("run_warnings").is_none(), "nothing runs until the user says yes: {allowed}");
}

#[test]
fn the_smoke_checks_that_wait_for_julias_asking_message_still_find_it() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "using Example")]);
    let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    let near = Snapshot::parse(&json!({
        "notebook_id": NB, "path": "/n/a.jl", "cell_order": [], "cells": [],
        "packages": { "step": "asking", "new": true, "download": { "packages": [{ "package": "Plts", "close_to": "Plots" }] } },
    })).unwrap();
    let messages = [ran["message"].as_str().unwrap().to_owned(), super::tools::asking_message(&near)];
    fn asked(check: &Value, into: &mut Vec<Value>) {
        if check["check"] == "asked_user" {
            into.push(check.clone());
        }
        for inner in check["checks"].as_array().into_iter().flatten() {
            asked(inner, into);
        }
    }
    let tasks = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../smoke/tasks");
    let mut found = Vec::new();
    for task in std::fs::read_dir(&tasks).unwrap() {
        let path = task.unwrap().path().join("checks.json");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let checks: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut specs = Vec::new();
        asked(&checks, &mut specs);
        for phrase in specs.iter().flat_map(|c| c["when_reply"].as_array().cloned().unwrap_or_default()) {
            let phrase = phrase.as_str().unwrap();
            for message in &messages {
                assert!(message.contains(phrase), "{}: when_reply {phrase:?} isn't in the asking message: {message}", path.display());
            }
            found.push(path.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    found.sort();
    assert_eq!(found, ["L1-long-analysis", "N7-plot"], "the smoke tasks with a when_reply");
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
        assert!(other.contains("invalid_path::Notebook path must end in .jl (Julia), .R (R) or .py (Python, marimo): "), "{other}");
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
    s.notebooks.bind("r-session", "");
    s.call("r-session", "new_notebook", json!({ "path": format!("{}{SEP}endeavor-kind-test.jl", std::env::temp_dir().to_string_lossy().trim_end_matches(SEP)) })).unwrap();
    assert_eq!(s.engine.calls.lock().unwrap().iter().filter(|call| *call == "new").count(), 3);
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

    assert_eq!(s.notebooks.cancelled("7", &json!(5)), None, "no call under way");
    {
        let _call = s.notebooks.running("7", &json!(5), NB, false);
        assert_eq!(s.notebooks.cancelled("8", &json!(5)), None, "another session's call of the same id");
        assert_eq!(s.notebooks.cancelled("7", &json!(6)), None, "another call");
        assert_eq!(s.notebooks.cancelled("7", &json!(5)), Some((NB.to_owned(), false)));
    }
    assert_eq!(s.notebooks.cancelled("7", &json!(5)), None, "the call ended");

    s.notebooks.stop_for("7", NB, true);
    assert_eq!(interrupts(), 1);

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

    running(false);
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-2", "cells": [X] } }));
    running(true);
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "the user's later run of a cell the session ran");
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-3", "cells": [] } }));
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "a restart's run of a cell the session ran");
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "run": "run-4", "cells": [X] } }));
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "a quick run that already ended");
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.notified(&json!({ "method": "process_exited", "params": { "notebook_id": NB, "running": [X] } }));
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "after the process ended");
    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    s.notebooks.restart(NB, false).unwrap();
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 2, "after the app's Restart");
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

    s.engine.with(NB, |nb| (nb.cells[0].running, nb.cells[1].queued) = (true, true));
    let stopped = interrupt();
    assert_eq!(
        stopped,
        json!({ "notebook_id": NB, "interrupted": true, "stopped": [X], "dropped": [Y], "still_running": [], "process_status": "ready" })
    );
    assert_eq!(interrupts(), 1);
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["running"], &read["errored"]), (&json!(false), &json!(true)));

    s.engine.with(NB, |nb| {
        (nb.cells[0].running, nb.cells[1].queued) = (true, true);
        nb.stale_on_interrupt = true;
    });
    let stale = interrupt();
    assert_eq!((&stale["dropped"], &stale["stale_cells"]), (&json!([Y]), &json!([Y])), "{stale}");
    assert!(stale["message"].as_str().unwrap().contains("earlier run's output"), "{stale}");
    s.engine.with(NB, |nb| nb.stale_on_interrupt = false);

    s.engine.with(NB, |nb| nb.cells[2].running = true);
    let late = Instant::now() - Duration::from_secs_f64(super::tools::WAIT_SECONDS);
    let stuck = tool_json(s.notebooks.tool("7", "interrupt_notebook", &json!({ "notebook_id": NB }), &Folder::Process, late).unwrap());
    assert_eq!((&stuck["interrupted"], &stuck["stopped"], &stuck["still_running"]), (&json!(true), &json!([]), &json!([Z])));
    assert!(stuck["message"].as_str().unwrap().contains("haven't stopped"), "{stuck}");

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
    s.engine.with(NB, |nb| (nb.cells[0].running, nb.cells[1].queued) = (true, true));
    s.notebooks.stop_for("8", NB, false);
    assert_eq!(interrupts(), 0, "another session's Stop");
    s.notebooks.stop_for("7", NB, false);
    assert_eq!(interrupts(), 1, "the restart's run is session 7's");
    assert_eq!(restarted.get("running_again"), None, "nothing was running");

    s.engine.with(NB, |nb| nb.cells[0].running = true);
    let again = s.call("7", "restart_notebook", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(again["running_again"], json!([X]), "{again}");
    assert!(again["message"].as_str().unwrap().contains("the cell in `running_again` is running again") && again["message"].as_str().unwrap().contains("edit it"), "{again}");
    s.engine.with(NB, |nb| nb.cells[0].running = false);

    s.engine.with(NB, |nb| nb.safe_preview = true);
    let refused = s.call("7", "restart_notebook", json!({ "notebook_id": NB })).unwrap_err();
    assert!(refused.contains("execution_blocked"), "{refused}");

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

    let preview = s.call("9", "preview_package_update", json!({ "notebook_id": R })).unwrap();
    assert_eq!((&preview["status"], &preview["date"], &preview["current_date"]), (&json!("ready"), &json!("2026-10-17"), &json!("2026-06-01")), "{preview}");
    assert!(preview["message"].as_str().unwrap().contains("apply_package_update"), "{preview}");
    assert_eq!(preview.get("reason"), None, "{preview}");

    let failed = s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2000-01-01" })).unwrap();
    assert!(failed["status"] == "failed" && failed["reason"] == "no index" && failed["message"].as_str().unwrap().contains("problems"), "{failed}");

    let refused = s.call("9", "apply_package_update", json!({ "notebook_id": R, "date": "2026-10-17" })).unwrap_err();
    assert!(refused.contains("no_preview"), "{refused}");
    assert!(s.call("9", "apply_package_update", json!({ "notebook_id": R })).unwrap_err().contains("date"), "date is required");
    s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2026-10-17" })).unwrap();
    let applied = s.call("9", "apply_package_update", json!({ "notebook_id": R, "date": "2026-10-17" })).unwrap();
    assert_eq!((&applied["date"], &applied["changes"][0]["to"]), (&json!("2026-10-17"), &json!("1.2.1")), "{applied}");
    let listed = s.call("9", "list_notebooks", json!({})).unwrap();
    assert_eq!(listed.as_array().unwrap().iter().find(|nb| nb["notebook_id"] == R).unwrap()["package_date"], "2026-10-17");

    assert!(applied["message"].as_str().unwrap().contains("installs now"), "{applied}");

    ember.with(R, |nb| nb.safe_preview = true);
    s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2026-10-18" })).unwrap();
    let waiting = s.call("9", "apply_package_update", json!({ "notebook_id": R, "date": "2026-10-18" })).unwrap();
    assert!(waiting["message"].as_str().unwrap().contains("once the user allows the notebook to run"), "{waiting}");

    let earlier = s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2026-10-19" })).unwrap();
    assert_eq!((&earlier["status"], &earlier["date"], earlier.get("asked")), (&json!("ready"), &json!("2026-10-18"), None), "{earlier}");
    assert!(earlier["message"].as_str().unwrap().starts_with("2026-10-19 isn't published yet, so this is the newest day that is, 2026-10-18."), "{earlier}");
    assert!(!preview["message"].as_str().unwrap().contains("published"), "{preview}");

    let same = s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2026-10-16" })).unwrap();
    assert!(same["message"].as_str().unwrap().starts_with("Nothing would change: every package is already at its version"), "{same}");
    assert_eq!(same.get("packages"), None, "{same}");
    let empty = s.call("9", "preview_package_update", json!({ "notebook_id": R, "date": "2025-01-15" })).unwrap();
    let message = empty["message"].as_str().unwrap();
    assert!(message.starts_with("The notebook has no packages yet, so none would change.") && message.contains("still moves the notebook's date") && message.contains("every package added later"), "{empty}");
    assert_eq!(empty.get("packages"), None, "{empty}");

    let pinned = s.call("9", "preview_package_update", json!({ "notebook_id": R, "package": "dplyr", "version": "1.1.2" })).unwrap();
    assert_eq!((&pinned["status"], &pinned["date"], pinned.get("asked")), (&json!("ready"), &json!("2023-09-02"), None), "{pinned}");
    assert!(pinned["message"].as_str().unwrap().contains("moves every package"), "{pinned}");
    let applied = s.call("9", "apply_package_update", json!({ "notebook_id": R, "date": "2023-09-02" })).unwrap();
    assert_eq!(applied["date"], "2023-09-02", "{applied}");
    let never = s.call("9", "preview_package_update", json!({ "notebook_id": R, "package": "dplyr", "version": "0.0.1" })).unwrap();
    assert!(never["status"] == "failed" && never["reason"] == "CRAN has no dplyr 0.0.1" && never["message"].as_str().unwrap().contains("No package date"), "{never}");
    let other = s.call("9", "preview_package_update", json!({ "notebook_id": R, "package": "ggplot2" })).unwrap_err();
    assert!(other.contains("not_updatable"), "{other}");
    for (args, says) in [
        (json!({ "notebook_id": R, "package": "dplyr", "date": "2026-10-17" }), "not both"),
        (json!({ "notebook_id": R, "version": "1.1.2" }), "needs package"),
        (json!({ "notebook_id": R, "package": 1 }), "must be strings"),
    ] {
        let refused = s.call("9", "preview_package_update", args.clone()).unwrap_err();
        assert!(refused.contains("invalid_argument") && refused.contains(says), "{args}: {refused}");
    }

    s.engine.open(NB, "/n/a.jl", &[(Y, "y = 1")]);
    let julia = s.call("7", "preview_package_update", json!({ "notebook_id": NB })).unwrap_err();
    assert!(julia.contains("not_r_notebook"), "{julia}");
}

#[test]
fn set_input_sets_an_r_notebooks_input_and_reruns_or_stales_its_readers() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    const Z: &str = "33333333-3333-3333-3333-333333333333";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "n <- input_slider(1, 10)"), (Y, "y = n * 2"), (Z, "z = 3")]);
    ember.with(R, |nb| nb.cells[0].inputs = vec!["n: slider 1 to 10, value 1".into()]);
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("9", "list_notebooks", json!({})).unwrap();

    let read = s.call("9", "read_cell", json!({ "notebook_id": R, "cell_id": X })).unwrap();
    assert_eq!(read["inputs"], json!(["n: slider 1 to 10, value 1"]), "{read}");
    let other = s.call("9", "read_cell", json!({ "notebook_id": R, "cell_id": Z })).unwrap();
    assert_eq!(other.get("inputs"), None, "a cell with none has no `inputs`: {other}");

    s.seconds(1.0);
    let set = s.call("9", "set_input", json!({ "notebook_id": R, "name": "n", "value": 7, "wait_for_completion": true })).unwrap();
    assert_eq!((&set["mutation"]["type"], &set["mutation"]["cell_id"], &set["mutation"]["value"]), (&json!("set_input"), &json!(X), &json!(7)), "{set}");
    assert_eq!(set["affected_cells"], json!([Y]), "{set}");
    assert_eq!(set["execution"]["status"], "completed", "{set}");
    assert!(set["message"].as_str().unwrap().contains("rerun"), "{set}");
    assert_eq!(ember.calls.lock().unwrap().iter().filter(|c| *c == "run").count(), 0, "Ember reruns the readers itself: no second run");

    let bad = s.call("9", "set_input", json!({ "notebook_id": R, "name": "n", "value": "bad" })).unwrap_err();
    assert!(bad.contains("invalid_value") && bad.contains("from 1 to 10"), "{bad}");
    let none = s.call("9", "set_input", json!({ "notebook_id": R, "name": "z", "value": 1 })).unwrap_err();
    assert!(none.contains("input_refused"), "{none}");
    assert!(s.call("9", "set_input", json!({ "notebook_id": R, "name": "n" })).unwrap_err().contains("value"), "value is required");

    let late = s.call("9", "set_input", json!({ "notebook_id": R, "name": "n", "value": "late" })).unwrap();
    assert_eq!((&late["pending"], &late["execution"]["status"]), (&json!(true), &json!("running")), "{late}");
    assert!(late["message"].as_str().unwrap().contains("will be set when R finishes"), "{late}");

    ember.with(R, |nb| nb.lazy = true);
    let lazy = s.call("9", "set_input", json!({ "notebook_id": R, "name": "n", "value": 3 })).unwrap();
    assert_eq!(lazy["stale_cells"], json!([Y]), "{lazy}");
    assert!(lazy["message"].as_str().unwrap().contains("lazy mode"), "{lazy}");

    s.engine.open(NB, "/n/a.jl", &[(X, "w = 1")]);
    let julia = s.call("7", "set_input", json!({ "notebook_id": NB, "name": "w", "value": 1 })).unwrap_err();
    assert!(julia.contains("not_r_notebook"), "{julia}");
}

#[test]
fn set_input_cuts_the_text_of_a_dependent_that_reran_unless_it_was_edited_and_not_yet_run() {
    const R: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    const W: &str = "44444444-4444-4444-4444-444444444444";
    let s = setup();
    let ember = Arc::new(Engine { clock: s.clock.clone(), ..Default::default() });
    ember.open(R, "/n/b.R", &[(X, "n <- input_slider(1, 10)"), (Y, "y = n * 2 # wide table"), (W, "w = y # wide table")]);
    s.notebooks.add_engine(Backend::Ember, ember.clone());
    s.call("", "list_notebooks", json!({})).unwrap();
    s.call("", "run_all_cells", json!({ "notebook_id": R, "wait_for_completion": true })).unwrap();
    ember.with(R, |nb| nb.autorun = true);
    let text = |set: &Value, cell: &str| set["outputs"]["changed"].as_array().unwrap().iter().find(|c| c["cell_id"] == cell).unwrap_or_else(|| panic!("{set}"))["output_text"].as_str().unwrap().to_owned();

    s.seconds(1.0);
    let set = s.call("", "set_input", json!({ "notebook_id": R, "name": "n", "value": 7, "wait_for_completion": true })).unwrap();
    assert_eq!(text(&set, Y), "n\t".repeat(300), "a reader of the input, in full: {set}");
    assert!(text(&set, W).ends_with("… (cut; read_cell shows more)") && text(&set, W).len() < 250, "{set}");

    s.seconds(1.0);
    s.read("", R, W);
    s.edit("", R, W, "w = y + 1 # wide table");
    let set = s.call("", "set_input", json!({ "notebook_id": R, "name": "n", "value": 8, "wait_for_completion": true })).unwrap();
    assert_eq!(text(&set, W), "n\t".repeat(300), "edited and not yet run, so in full: {set}");
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

    let began = Instant::now();
    let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    assert!(began.elapsed() < Duration::from_secs(5), "the run's wait ended at the prompt");
    assert_eq!(ran["waiting_for_input"], json!({ "cell_id": X, "prompt_id": 1, "prompt": "Name? ", "choices": [] }), "{ran}");
    assert_eq!((&ran["execution"]["status"], &ran["execution"]["still_running"]), (&json!("running"), &json!([X])), "{ran}");
    let warnings = ran["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|w| w.as_str().unwrap().starts_with("waiting_for_input::")) && !warnings.iter().any(|w| w.as_str().unwrap().starts_with("execution_timeout::")), "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("answer_prompt") && ran["message"].as_str().unwrap().contains("ask the user first"), "{ran}");

    let began = Instant::now();
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert!(began.elapsed() < Duration::from_secs(2), "no wait for a cell waiting on a prompt");
    assert_eq!((&read["running"], &read["waiting_for_input"]["prompt"]), (&json!(true), &json!("Name? ")), "{read}");
    assert!(read["message"].as_str().unwrap().contains("answer_prompt"), "{read}");

    let refused = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": Y, "text": "Ada" })).unwrap_err();
    assert!(refused.contains("no_prompt::"), "{refused}");
    let refused = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": X, "text": 3 })).unwrap_err();
    assert!(refused.contains("invalid_argument::"), "{refused}");
    let answers = || s.engine.calls.lock().unwrap().iter().filter(|m| *m == "answer").count();
    let refused = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": X, "text": "Ada", "prompt_id": 7 })).unwrap_err();
    assert!(refused.contains("stale_prompt::"), "{refused}");
    assert_eq!(answers(), 0, "nothing reached the engine");

    let answered = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": X, "text": "Ada", "prompt_id": 1, "wait_for_completion": true })).unwrap();
    assert_eq!(answered["execution"]["status"], json!("completed"), "{answered}");
    assert_eq!(answered["outputs"]["changed"][0]["output_summary"], json!("answered Ada"), "{answered}");
    assert_eq!(answered.get("waiting_for_input"), None);
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["running"], read.get("waiting_for_input")), (&json!(false), None), "{read}");

    let ran = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": Z, "wait_for_completion": true })).unwrap();
    assert_eq!(ran["waiting_for_input"]["choices"], json!(["Yes", "No"]), "{ran}");
    assert!(ran["message"].as_str().unwrap().contains("a choice's number or its text"), "{ran}");
    let answered = s.call("7", "answer_prompt", json!({ "notebook_id": NB, "cell_id": Z, "text": "1" })).unwrap();
    assert_eq!(answered["execution"]["status"], json!("running"), "{answered}");

    s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": X, "wait_for_completion": true })).unwrap();
    let stopped = s.call("7", "interrupt_notebook", json!({ "notebook_id": NB })).unwrap();
    assert_eq!(stopped["stopped"], json!([X]), "{stopped}");
    let read = s.call("7", "read_cell", json!({ "notebook_id": NB, "cell_id": X })).unwrap();
    assert_eq!((&read["errored"], read.get("waiting_for_input")), (&json!(true), None), "{read}");
}

#[test]
fn a_yes_covers_the_packages_the_user_saw_that_the_call_names() {
    let seen = json!(["CSV", "typing-extensions"]);
    assert_eq!(seen_packages(None, &seen), seen, "a call that names none: what the user saw");
    assert_eq!(seen_packages(Some(&json!(["csv", "JSON"])), &seen), json!(["CSV"]), "never one the user didn't see");
    assert_eq!(seen_packages(Some(&json!(["Typing_Extensions"])), &seen), json!(["typing-extensions"]), "as PyPI compares names");
    assert_eq!(seen_packages(Some(&json!(["Example"])), &seen), json!([]));
    assert_eq!(seen_packages(Some(&json!("CSV")), &seen), seen, "not a list: ignored");
}

#[test]
fn an_idle_limit_reads_in_hours_or_under_an_hour_in_minutes() {
    assert_eq!(super::idle_span(48.0), "48 hours");
    assert_eq!(super::idle_span(1.0), "1 hour");
    assert_eq!(super::idle_span(2.5), "2 hours", "rounded as Julia rounds");
    assert_eq!(super::idle_span(60.0 / 3600.0), "1 minute");
    assert_eq!(super::idle_span(0.5), "30 minutes");
    assert_eq!(super::idle_span(0.0001), "1 minute", "never 0");
}
