//! The core's notebook tools and state against a fake engine: an adapter that
//! keeps notebooks in memory, analyses `name = expression` cells, and runs a
//! cell by stamping it with the test's clock.

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
}

impl FakeCell {
    fn new(id: &str, code: &str) -> FakeCell {
        FakeCell { id: id.into(), code: code.into(), folded: false, running: false, queued: false, errored: false, last_run: 0.0, output: String::new(), hidden: false }
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
}

impl Engine {
    fn open(&self, id: &str, path: &str, cells: &[(&str, &str)]) {
        let cells = cells.iter().map(|(id, code)| FakeCell::new(id, code)).collect();
        self.notebooks.lock().unwrap().push(FakeNotebook { id: id.into(), path: path.into(), cells, safe_preview: false, exited: None });
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
        json!({
            "notebook_id": nb.id, "path": nb.path,
            "process_status": if nb.safe_preview { "waiting_for_permission" } else if nb.exited.is_some() { "no_process" } else { "ready" },
            "execution_allowed": !nb.safe_preview && nb.exited.is_none(), "safe_preview": nb.safe_preview, "exited": nb.exited,
            "cell_order": nb.cells.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            "cells": nb.cells.iter().map(|c| json!({
                "cell_id": c.id, "code": c.code, "folded": c.folded, "running": c.running, "queued": c.queued, "errored": c.errored,
                "last_run": c.last_run, "runtime": 0, "output": c.output, "hidden": c.hidden, "markdown": c.code.starts_with("md\""),
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
        let now = *self.clock.lock().unwrap();
        let mut notebooks = self.notebooks.lock().unwrap();
        match method {
            "status" => return Ok(json!({ "pluto": "running" })),
            "open" | "new" => {
                let path = params["path"].as_str().map_or_else(|| format!("{}/made.jl", params["folder"].as_str().unwrap_or("/n")), str::to_owned);
                let mut made = self.made.lock().unwrap();
                *made += 1;
                let id = format!("cccccccc-0000-0000-0000-{:012}", *made);
                let cells = if method == "new" { vec![FakeCell::new(&format!("dddddddd-0000-0000-0000-{:012}", *made), "")] } else { Vec::new() };
                let listed: Vec<Value> = cells.iter().map(|c| json!({ "cell_id": c.id, "code": c.code })).collect();
                notebooks.push(FakeNotebook { id: id.clone(), path: path.clone(), cells, safe_preview: params["run"] == false, exited: None });
                return Ok(json!({ "notebook_id": id, "path": path, "process_status": "starting", "cells": listed }));
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
                for cell in nb.cells.iter_mut().filter(|c| cells.contains(&c.id)) {
                    cell.last_run = now;
                    cell.errored = cell.code.contains("error(");
                    cell.output = if cell.errored { "boom".into() } else { format!("ran {}", cell.code) };
                }
                let mut reply = json!({ "accepted": true, "process_status": "ready" });
                if params["wait"] == true {
                    reply["completed"] = json!(cells);
                    reply["timed_out"] = json!([]);
                }
                Ok(reply)
            }
            "allow_execution" => {
                if !nb.safe_preview {
                    return Ok(json!({ "already_allowed": true, "ran": false, "process_status": "ready" }));
                }
                nb.safe_preview = false;
                Ok(json!({ "already_allowed": false, "ran": params["run"], "process_status": if params["run"] == true { "starting" } else { "ready" } }))
            }
            "render_text" => Ok(json!({ "text": if { let at = find(nb, &params["cell_id"]); nb.cells[at].code.contains("table") } { json!("n\tmean\n3\t2.5") } else { Value::Null } })),
            "render_png" => Ok(json!({ "png": if { let at = find(nb, &params["cell_id"]); nb.cells[at].code.contains("plot") } { json!("iVBORw==") } else { Value::Null }, "mime": "text/plain" })),
            "validate" => Ok(json!({ "errors": if params["code"].as_str().unwrap().contains('\n') { json!([{ "type": "pluto_multi_expression" }]) } else { json!([]) } })),
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
    let notebooks = Arc::new(Notebooks::new(engine.clone(), Box::new(move || *now.lock().unwrap())));
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
        if let Some(refusal) = self.notebooks.refusal(owner, tool, &arguments) {
            return Err(refusal);
        }
        if tool == "keep_notebook_alive" {
            return self.notebooks.keep_alive(&arguments);
        }
        self.notebooks.note_activity(&arguments);
        self.notebooks.tool(owner, tool, &arguments, None).map(tool_json)
    }

    /// The kind of error a call failed with (`read_required`, ...).
    fn refused(&self, owner: &str, tool: &str, arguments: Value) -> String {
        let error = self.call(owner, tool, arguments).expect_err("refused");
        let text: Value = serde_json::from_str(crate::mcp::tool_error(&error)["content"][0]["text"].as_str().unwrap()).unwrap();
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
    assert_eq!(first, r#"{"cells":{},"idle_stopped":[],"notebooks":[]}"#, "the current state, on connect");

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
    assert_eq!((&receipt["cell_order"], &receipt["execution_order"]), (&json!([X, Y]), &json!([X, Y])));
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
    assert_eq!(receipt["warnings"], json!(["async_execution::cells running; pending_run clears when execution finishes"]));
    s.seconds(1.0);
    s.edit("", NB, X, "x = 12");
    s.notebooks.notified(&json!({ "method": "run_finished", "params": { "notebook_id": NB, "cells": [X] } }));
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([]));
    s.seconds(1.0);
    s.edit("", NB, X, "x = 13");
    s.seconds(1.0);
    s.engine.with(NB, |nb| nb.cells[0].last_run = 1.0e6 + 3.5);
    assert_eq!(s.call("", "list_notebooks", json!({})).unwrap()[0]["pending_run"], json!([]), "Pluto's own run button");
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
    assert_eq!(receipt["mutation"], json!({ "type": "edit_cells", "cell_ids": [X, Y] }));
    assert_eq!((&receipt["pending_run"], &receipt["execution"]["status"]), (&json!([X, Y]), &json!("staged")));
}

#[test]
fn adding_moving_folding_and_deleting_cells() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[]);
    let add = |args: Value| s.call("", "add_cell", args);
    let first = add(json!({ "notebook_id": NB, "code": "a = 1" })).unwrap();
    let a = first["cell_id"].as_str().unwrap().to_owned();
    assert_eq!((&first["code"], &first["code_folded"], &first["pending_run"]), (&json!("a = 1"), &json!(false), &json!([a])));
    assert_eq!(s.refused("", "add_cell", json!({ "notebook_id": NB, "code": "b = a" })), "placement_required");
    assert_eq!(s.refused("", "add_cell", json!({ "notebook_id": NB, "code": "b = a", "after_cell_id": "" })), "placement_required");
    assert_eq!(s.refused("", "add_cell", json!({ "notebook_id": NB, "code": "b = a", "after_cell_id": a, "folded": "yes" })), "invalid_argument");
    // The agent read the cell it added: it can add after it, and edit it at once.
    let b = add(json!({ "notebook_id": NB, "code": "b = a", "after_cell_id": a, "folded": true })).unwrap();
    assert_eq!((&b["code_folded"], &b["execution"]["status"]), (&json!(true), &json!("staged")));
    let b = b["cell_id"].as_str().unwrap().to_owned();
    let c = add(json!({ "notebook_id": NB, "code": 42, "after_cell_id": a, "run_after": true })).unwrap();
    assert_eq!((&c["code"], &c["affected_cells"], &c["execution"]["status"]), (&json!("42"), &json!([c["cell_id"]]), &json!("running")));
    let c = c["cell_id"].as_str().unwrap().to_owned();
    assert_eq!(s.engine.order(NB), [a.clone(), c.clone(), b.clone()]);
    s.edit("", NB, &b, "b = a + 1");

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
    assert_eq!(*s.engine.calls.lock().unwrap().iter().rev().nth(2).unwrap(), "run", "Pluto's cleanup after a delete");
    assert_eq!(s.refused("", "delete_cell", json!({ "notebook_id": NB, "cell_id": b })), "cell_not_found");
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
    let with_markdown = read(json!({ "notebook_id": NB, "order": "visual", "include_markdown": true }));
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
    let blocked = "execution_blocked::notebook is not running code (process_status=waiting_for_permission); pending_run kept; call allow_execution to exit safe preview";
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
    let other = |result: &Value| result["warnings"].as_array().unwrap().iter().filter(|w| w.as_str().unwrap().starts_with("other_session::")).cloned().collect::<Vec<_>>();
    for owner in ["A", "B"] {
        call(owner, "read_notebook_code", json!({})).unwrap();
    }
    let edited = call("B", "edit_cell", json!({ "cell_id": a, "code": "a = 2" })).unwrap();
    assert!(other(&edited).is_empty());

    // B's edit doesn't count as A's read of the cell.
    assert_eq!(s.refused("A", "edit_cell", json!({ "notebook_id": NB, "cell_id": a, "code": "a = 3" })), "stale_read");

    s.seconds(30.0);
    let unrelated = call("A", "edit_cell", json!({ "cell_id": c, "code": "c = 11" })).unwrap();
    assert_eq!(other(&unrelated), [json!(format!("other_session::Another Endeavor session changed {a} in this notebook 30 s ago. Read cells before relying on them."))]);

    let conflict = format!(
        "ArgumentError: run_conflict::Another Endeavor session changed {a} since you last read them, and the cells you're running depend on them. \
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

    s.seconds(91.0);
    let expired = call("A", "fold_cell", json!({ "cell_id": c, "folded": true })).unwrap();
    assert!(other(&expired).is_empty());

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
    let view = |cell: &str| s.notebooks.tool("", "view_cell_output", &json!({ "notebook_id": NB, "cell_id": cell }), None);
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
fn opening_and_making_notebooks() {
    let s = setup();
    let dir = temp_notebooks("open", 1)[0].rsplit_once('/').unwrap().0.to_owned();
    let path = format!("{dir}/nb0.jl");
    let opened = s.call("", "open_notebook", json!({ "path": path, "run_notebook": true })).unwrap();
    assert_eq!(opened["warnings"], json!(["async_execution::open queued non-blocking notebook run; poll read_cell for completion"]));
    assert_eq!((&opened["execution_allowed"], &opened["ran"], &opened["process_status"]), (&json!(true), &json!(true), &json!("starting")));
    let previewed = s.call("", "open_notebook", json!({ "path": path })).unwrap();
    assert_eq!((&previewed["execution_allowed"], previewed.get("warnings")), (&json!(false), None));
    assert_eq!(s.call("", "open_notebook", json!({ "path": format!("{dir}/none.jl") })), Err(format!("ArgumentError: file_not_found::No file at '{dir}/none.jl'")));
    assert_eq!(s.call("", "open_notebook", json!({})), Err("ArgumentError: invalid_path::path is required".into()));
    assert_eq!(s.call("", "open_notebook", json!({ "path": path, "run_notebook": "yes" })), Err("TypeError: non-boolean (String) used in boolean context".into()));

    let made = s.call("", "new_notebook", json!({ "path": format!("{dir}/./fresh.jl") })).unwrap();
    assert_eq!((&made["path"], &made["created"], &made["ran"]), (&json!(format!("{dir}/fresh.jl")), &json!(true), &json!(true)));
    // Its empty first cell can be edited straight away, without a read first.
    s.edit("", made["notebook_id"].as_str().unwrap(), made["cell_ids"][0].as_str().unwrap(), "x = 1");
    assert_eq!(s.call("", "new_notebook", json!({ "path": path })), Err(format!("ArgumentError: file_exists::'{path}' already exists; use open_notebook to load it")));
    assert_eq!(s.refused("", "new_notebook", json!({ "path": format!("{dir}/x.txt") })), "invalid_path");
    assert_eq!(s.call("", "new_notebook", json!({ "path": format!("{dir}/missing/y.jl") })), Err(format!("ArgumentError: invalid_path::Directory does not exist: '{dir}/missing'")));
    // A session's folder takes its unnamed notebooks, and relative paths.
    let named = s.notebooks.tool("s", "new_notebook", &json!({ "path": "named.jl" }), Some(&dir)).map(tool_json).unwrap();
    assert_eq!(named["path"], format!("{dir}/named.jl"));
    let unnamed = s.notebooks.tool("t", "new_notebook", &json!({}), Some(&dir)).map(tool_json).unwrap();
    assert_eq!(unnamed["path"], format!("{dir}/made.jl"));
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
    let dir = dir.canonicalize().unwrap();
    (0..count)
        .map(|i| {
            let path = dir.join(format!("nb{i}.jl"));
            std::fs::write(&path, "").unwrap();
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
             You can still read other notebooks as plain .jl files. To work on another notebook, suggest the user start a new session with it."
        )
    );
    assert!(s.call("a", "new_notebook", json!({})).unwrap_err().contains("so it can't create another notebook."));
    assert!(s.call("a", "new_notebook", json!({ "path": "/elsewhere/other.jl" })).unwrap_err().contains("so it can't create /elsewhere/other.jl."));
    assert!(s.call("a", "open_notebook", json!({})).unwrap_err().contains("so it can't create another notebook."), "as Julia said it");
    // Its own notebook isn't refused, however it's written.
    let roundabout = format!("{first_nb}/../{}", first_nb.rsplit('/').next().unwrap());
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": roundabout })), None);
    let relative = pathdiff(first_nb, &std::env::current_dir().unwrap().canonicalize().unwrap().display().to_string());
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": relative })), None);
    assert_eq!(s.notebooks.refusal("a", "open_notebook", &json!({ "path": "~bob/x.jl" })), Some("ArgumentError: ~user tilde expansion not yet implemented".into()));

    // Calls without an owner (the app, tests) are unrestricted.
    let second_id = s.call("", "open_notebook", json!({ "path": second_nb })).unwrap()["notebook_id"].as_str().unwrap().to_owned();
    s.notebooks.publish();

    // Writes and runs on another open notebook are refused; reads are not.
    assert_eq!(refused(s.call("a", "edit_cell", json!({ "notebook_id": second_id, "cell_id": X, "code": "1" }))), one);
    assert_eq!(
        s.call("a", "run_all_cells", json!({ "notebook_id": second_id })).unwrap_err(),
        format!(
            "ArgumentError: one_notebook::This session works on one notebook, {first_nb}, so it can't change or run {second_nb}. \
             You can still read other notebooks as plain .jl files. To work on another notebook, suggest the user start a new session with it."
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
    let dir = paths[0].rsplit_once('/').unwrap().0.to_owned();
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
    let renamed = format!("{dir}/renamed.jl");
    assert_eq!(s.notebooks.move_notebook(&id(1), &renamed), Ok(json!({ "path": renamed })));
    assert_eq!(s.notebooks.bound("a"), Some(renamed.clone()));
    assert_eq!(s.notebooks.idle_stopped(), [json!({ "path": renamed, "hours": 1, "safe_preview": false })]);
    assert_eq!(s.notebooks.refusal("a", "edit_cell", &json!({ "notebook_id": id(1) })), None, "still its own notebook");
    assert_eq!(s.notebooks.move_notebook(&id(1), &paths[1]), Err(format!("ArgumentError: file_exists::'{}' already exists", paths[1])));
    assert_eq!(s.notebooks.move_notebook(&id(1), &format!("{dir}/notes.txt")), Err(format!("ArgumentError: invalid_path::Notebook path must end in .jl: '{dir}/notes.txt'")));
    assert_eq!(s.notebooks.move_notebook(&id(1), &format!("{dir}/gone/x.jl")), Err(format!("ArgumentError: invalid_path::Directory does not exist: '{dir}/gone'")));

    // Whether a file is there, and when it last changed, as Julia's mtime says.
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(&paths[1]).unwrap();
    let modified = meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9;
    assert_eq!(file_info(&paths[1]), Ok(json!({ "exists": true, "modified": modified })));
    assert_eq!(file_info(&paths[0]), Ok(json!({ "exists": false })));
    assert_eq!(file_info(&dir), Ok(json!({ "exists": false })), "a folder isn't a file");

    // The app's New notebook for a session: in its folder, and the session's notebook from now on.
    s.notebooks.bind("b", &renamed);
    let made = s.notebooks.new_for("b", Some(&dir)).unwrap();
    assert_eq!(made["path"], format!("{dir}/made.jl"));
    assert_eq!(s.notebooks.bound("b"), Some(format!("{dir}/made.jl")));
}

/// `path` relative to `from`, through `..`s.
fn pathdiff(path: &str, from: &str) -> String {
    let (path, from): (Vec<_>, Vec<_>) = (path.split('/').filter(|p| !p.is_empty()).collect(), from.split('/').filter(|p| !p.is_empty()).collect());
    let common = path.iter().zip(&from).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; from.len() - common];
    parts.extend(&path[common..]);
    parts.join("/")
}

#[test]
fn stop_notebook_shuts_it_down_and_says_if_it_was_in_safe_preview() {
    let s = setup();
    let paths = temp_notebooks("stop", 2);
    s.engine.open(&id(1), &paths[0], &[]);
    s.engine.with(&id(1), |nb| nb.safe_preview = true);
    s.engine.open(&id(2), &format!("{}/../{}", paths[1].rsplit_once('/').unwrap().0, "nb1.jl"), &[]);
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
    let text = |raw: &str| crate::mcp::tool_error(raw)["content"][0]["text"].as_str().unwrap().to_owned();
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
    let dir = temp_notebooks("paths", 1)[0].rsplit_once('/').unwrap().0.to_owned();
    assert_eq!(canonical_path(&format!("{dir}/./nb0.jl")).unwrap(), format!("{dir}/nb0.jl"));
    assert_eq!(canonical_path(&format!("{dir}/new.jl")).unwrap(), format!("{dir}/new.jl"), "a file yet to be made");
    assert_eq!(canonical_path("/no/such/dir/../x.jl").unwrap(), "/no/such/x.jl");
    assert_eq!(canonical_path("~").unwrap(), std::fs::canonicalize(home()).unwrap().display().to_string());
    // /tmp is a symlink on macOS: paths resolve through it.
    if let Ok(real) = std::fs::canonicalize("/tmp") {
        assert_eq!(canonical_path("/tmp/endeavor-no-such.jl").unwrap(), format!("{}/endeavor-no-such.jl", real.display()));
    }
}

#[test]
fn a_notebooks_own_julia_ending_by_itself_ends_the_run_claude_waits_for() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(X, "x = 6"), (Y, "rates = crash()")]);
    let (_, rx) = s.notebooks.subscribe().unwrap();

    let error = s.call("7", "execute_cell", json!({ "notebook_id": NB, "cell_id": Y, "wait_for_completion": true })).expect_err("the run ended");
    let shown: Value = serde_json::from_str(crate::mcp::tool_error(&error)["content"][0]["text"].as_str().unwrap()).unwrap();
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
