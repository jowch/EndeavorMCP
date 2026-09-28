//! The core's notebook state against a fake engine: an adapter answering
//! `snapshot`, `graph` and `shutdown` from notebooks the test sets up, and
//! tool calls the test answers.

use super::*;

const X: &str = "11111111-1111-1111-1111-111111111111";
const Y: &str = "22222222-2222-2222-2222-222222222222";
const Z: &str = "33333333-3333-3333-3333-333333333333";

struct FakeCell {
    id: String,
    code: String,
    defines: Vec<&'static str>,
    running: bool,
    queued: bool,
    errored: bool,
}

struct FakeNotebook {
    id: String,
    path: String,
    cells: Vec<FakeCell>,
    pending: Vec<String>,
    safe_preview: bool,
}

type Tool = Box<dyn FnMut(&mut Vec<FakeNotebook>, &str, &Value) -> Result<Value, String> + Send>;

#[derive(Default)]
struct Engine {
    notebooks: Mutex<Vec<FakeNotebook>>,
    tool: Mutex<Option<Tool>>,
    shut_down: Mutex<Vec<String>>,
    /// A tool call with `"hold": true` says it has started, then waits here
    /// before the engine answers it, like an edit waiting for its run.
    hold: Mutex<Option<(Sender<()>, Receiver<()>)>>,
}

impl Engine {
    fn open(&self, id: &str, path: &str, cells: &[(&str, &str, &[&'static str])]) {
        let cells = cells.iter().map(|(id, code, defines)| FakeCell { id: id.to_string(), code: code.to_string(), defines: defines.to_vec(), running: false, queued: false, errored: false }).collect();
        self.notebooks.lock().unwrap().push(FakeNotebook { id: id.into(), path: path.into(), cells, pending: Vec::new(), safe_preview: false });
    }

    fn with<T>(&self, id: &str, f: impl FnOnce(&mut FakeNotebook) -> T) -> T {
        f(self.notebooks.lock().unwrap().iter_mut().find(|nb| nb.id == id).unwrap())
    }

    fn snapshot(nb: &FakeNotebook) -> Value {
        json!({
            "notebook_id": nb.id, "path": nb.path, "execution_allowed": !nb.safe_preview, "safe_preview": nb.safe_preview,
            "cell_order": nb.cells.iter().map(|c| c.id.clone()).collect::<Vec<_>>(), "pending_run": nb.pending,
            "cells": nb.cells.iter().map(|c| json!({ "cell_id": c.id, "code": c.code, "running": c.running, "queued": c.queued, "errored": c.errored })).collect::<Vec<_>>(),
        })
    }

    fn adapter(&self, method: &str, params: &Value) -> Result<Value, String> {
        let mut notebooks = self.notebooks.lock().unwrap();
        let Some(id) = params["notebook_id"].as_str() else {
            return Ok(json!({ "notebooks": notebooks.iter().map(Engine::snapshot).collect::<Vec<_>>() }));
        };
        let Some(at) = notebooks.iter().position(|nb| nb.id == id) else { return Err(format!("KeyError: no {id}")) };
        match method {
            "snapshot" => Ok(Engine::snapshot(&notebooks[at])),
            "graph" => Ok(json!({ "cells": notebooks[at].cells.iter().map(|c| json!({ "cell_id": c.id, "definitions": c.defines, "functions": [] })).collect::<Vec<_>>() })),
            "shutdown" => {
                let nb = notebooks.remove(at);
                self.shut_down.lock().unwrap().push(nb.id);
                Ok(json!({ "safe_preview": nb.safe_preview }))
            }
            _ => Err("unknown".into()),
        }
    }
}

impl Upstream for Engine {
    fn ask(&self, path: &str, raw: &[u8], _: &Caller) -> io::Result<String> {
        let message: Value = serde_json::from_slice(raw).unwrap();
        let reply = match path {
            "/adapter" => match self.adapter(message["method"].as_str().unwrap(), &message["params"]) {
                Ok(result) => json!({ "result": result }),
                Err(error) => json!({ "error": error }),
            },
            _ => {
                if message["params"]["arguments"]["hold"] == true {
                    let (started, release) = self.hold.lock().unwrap().take().expect("a hold");
                    started.send(()).unwrap();
                    release.recv().unwrap();
                }
                let mut tool = self.tool.lock().unwrap();
                let mut notebooks = self.notebooks.lock().unwrap();
                let params = &message["params"];
                let result = tool.as_mut().expect("a tool")(&mut notebooks, params["name"].as_str().unwrap(), &params["arguments"]);
                let (text, error) = match result {
                    Ok(result) => (result.to_string(), false),
                    Err(error) => (json!({ "error": error }).to_string(), true),
                };
                json!({ "id": 1, "jsonrpc": "2.0", "result": { "content": [{ "type": "text", "text": text }], "isError": error } })
            }
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
    let engine = Arc::new(Engine::default());
    let clock = Arc::new(Mutex::new(1.0e6));
    let now = clock.clone();
    let notebooks = Arc::new(Notebooks::new(engine.clone(), Box::new(move || *now.lock().unwrap())));
    Setup { engine, notebooks, clock }
}

impl Setup {
    fn hours(&self, hours: f64) {
        *self.clock.lock().unwrap() += hours * 3600.0;
    }

    /// Answer tool calls with `tool`.
    fn tools(&self, tool: impl FnMut(&mut Vec<FakeNotebook>, &str, &Value) -> Result<Value, String> + Send + 'static) {
        *self.engine.tool.lock().unwrap() = Some(Box::new(tool));
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
        let raw = json!({ "params": { "name": tool, "arguments": arguments } }).to_string();
        let reply = self.notebooks.forward_tool(owner, tool, &arguments, || self.engine.ask("/dispatch", raw.as_bytes(), &Caller::default())).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        let text: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if reply["result"]["isError"] == true { Err(text["error"].as_str().unwrap().to_owned()) } else { Ok(text) }
    }
}

/// The next event on `rx`, parsed, within a second.
fn next(rx: &Receiver<String>) -> Value {
    serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).expect("an event")).unwrap()
}

fn cell<'a>(event: &'a Value, notebook: &str, cell: &str) -> &'a Value {
    event["cells"][notebook].as_array().unwrap().iter().find(|c| c["cell_id"] == cell).unwrap()
}

/// Tools that edit the fake notebook the way Julia's do, staging each edit.
fn editing_tools(nb: &'static str) -> impl FnMut(&mut Vec<FakeNotebook>, &str, &Value) -> Result<Value, String> + Send {
    move |notebooks, tool, args| {
        let notebook = notebooks.iter_mut().find(|n| n.id == nb).unwrap();
        match tool {
            "edit_cell" => {
                let id = args["cell_id"].as_str().unwrap();
                let code = args["code"].as_str().unwrap();
                notebook.cells.iter_mut().find(|c| c.id == id).unwrap().code = code.into();
                notebook.pending.push(id.into());
                Ok(json!({ "cell_id": id, "code": code, "applied": true }))
            }
            "edit_cells" => {
                let mut ids = Vec::new();
                for edit in args["cells"].as_array().unwrap() {
                    let id = edit["cell_id"].as_str().unwrap();
                    notebook.cells.iter_mut().find(|c| c.id == id).unwrap().code = edit["code"].as_str().unwrap().into();
                    notebook.pending.push(id.into());
                    ids.push(id);
                }
                Ok(json!({ "mutation": { "type": "edit_cells", "cell_ids": ids } }))
            }
            "add_cell" => {
                let code = args["code"].as_str().unwrap();
                notebook.cells.push(FakeCell { id: Z.into(), code: code.into(), defines: vec!["z"], running: false, queued: false, errored: false });
                notebook.pending.push(Z.into());
                Ok(json!({ "cell_id": Z, "code": code }))
            }
            _ => Ok(json!({})),
        }
    }
}

const NB: &str = "aaaaaaaa-0000-0000-0000-000000000001";

#[test]
fn events_say_who_changed_each_cell_and_what_the_agent_replaced() {
    let s = setup();
    let (first, rx) = s.notebooks.subscribe().unwrap();
    assert_eq!(first, r#"{"cells":{},"idle_stopped":[],"notebooks":[]}"#, "the current state, on connect");

    s.engine.open(NB, "/n/a.jl", &[(X, "x = 6", &["x"]), (Y, "y = x * 7", &["y"])]);
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!(event["notebooks"], json!([{ "notebook_id": NB, "path": "/n/a.jl", "cell_count": 2, "pending_run": [], "running": [], "execution_allowed": true }]));
    assert_eq!(
        cell(&event, NB, Y),
        &json!({ "cell_id": Y, "running": false, "errored": false, "unrun": false, "author": null, "before": null, "version": format!("{:x}", hash("y = x * 7")), "name": "y" })
    );
    s.notebooks.publish();
    assert!(rx.try_recv().is_err(), "nothing changed, nothing sent");

    // An agent edit: unrun, authored by the agent, with the code it replaced.
    s.tools(editing_tools(NB));
    s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": Y, "code": "y = x * 8" })).unwrap();
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!((&cell(&event, NB, Y)["author"], &cell(&event, NB, Y)["before"], &cell(&event, NB, Y)["unrun"]), (&json!("agent"), &json!("y = x * 7"), &json!(true)));
    assert_eq!(cell(&event, NB, Y)["version"], format!("{:x}", hash("y = x * 8")));
    assert_eq!(event["notebooks"][0]["pending_run"], json!([Y]));

    // A later edit keeps the first before-text; once the cell runs it's forgotten.
    s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": Y, "code": "y = x * 9" })).unwrap();
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, Y)["before"], "y = x * 7");
    s.engine.with(NB, |nb| nb.pending.clear());
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!((&cell(&event, NB, Y)["author"], &cell(&event, NB, Y)["before"], &cell(&event, NB, Y)["unrun"]), (&json!("agent"), &Value::Null, &json!(false)));
    s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": Y, "code": "y = x * 10" })).unwrap();
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, Y)["before"], "y = x * 9", "a new before-text after the run");

    // A change the tools didn't make (Pluto's editor submitting code): the user's.
    s.engine.with(NB, |nb| nb.cells[0].code = "x = 5".into());
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, X)["author"], "user");

    // Seen as each state comes: an edit undone before the next event still counts.
    s.engine.with(NB, |nb| nb.pending.clear());
    s.notebooks.publish();
    let _ = next(&rx);
    let cells = |code: &str| json!({ "method": "cell_state", "params": { "notebook_id": NB, "cells": [{ "cell_id": Y, "code": code }] } });
    assert!(s.notebooks.notified(&cells("y = 1")));
    assert!(s.notebooks.notified(&cells("y = x * 10")));
    s.notebooks.publish();
    assert_eq!(cell(&next(&rx), NB, Y)["author"], "user");

    // A cell the agent adds: its before-text is empty. Several edits at once.
    s.call("7", "add_cell", json!({ "notebook_id": NB, "after_cell_id": Y, "code": "z = y + 1" })).unwrap();
    s.call("7", "edit_cells", json!({ "notebook_id": NB, "cells": [{ "cell_id": X, "code": "x = 1" }] })).unwrap();
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!(cell(&event, NB, Z), &json!({ "cell_id": Z, "running": false, "errored": false, "unrun": true, "author": "agent", "before": "", "version": format!("{:x}", hash("z = y + 1")), "name": "z" }));
    assert_eq!((&cell(&event, NB, X)["author"], &cell(&event, NB, X)["before"]), (&json!("agent"), &json!("x = 5")));

    // Queued cells count as running only while something runs; names join the first three definitions.
    s.engine.with(NB, |nb| {
        nb.cells[0].queued = true;
        nb.cells[1].defines = vec!["f", "b", "a", "c"];
    });
    s.notebooks.publish();
    let event = next(&rx);
    assert_eq!((&event["notebooks"][0]["running"], &cell(&event, NB, Y)["name"]), (&json!([]), &json!("a, b, c")));
    s.engine.with(NB, |nb| nb.cells[1].running = true);
    s.notebooks.publish();
    assert_eq!(next(&rx)["notebooks"][0]["running"], json!([X, Y]));
}

#[test]
fn an_edit_waiting_for_its_run_holds_up_no_other_edit() {
    let s = setup();
    let (a, b) = (id(1), id(2));
    s.engine.open(&a, "/n/a.jl", &[(X, "x = 1", &[]), (Y, "y = 1", &[])]);
    s.engine.open(&b, "/n/b.jl", &[(X, "x = 1", &[])]);
    s.tools(|notebooks, _, args| {
        let notebook = notebooks.iter_mut().find(|n| n.id == args["notebook_id"].as_str().unwrap()).unwrap();
        let cell = args["cell_id"].as_str().unwrap();
        notebook.cells.iter_mut().find(|c| c.id == cell).unwrap().code = args["code"].as_str().unwrap().into();
        notebook.pending.push(cell.into());
        Ok(json!({ "cell_id": cell, "code": args["code"] }))
    });
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *s.engine.hold.lock().unwrap() = Some((started_tx, release_rx));
    let (s, a, b) = (&s, &a, &b);
    std::thread::scope(|scope| {
        let held = scope.spawn(|| s.call("7", "edit_cell", json!({ "notebook_id": a, "cell_id": X, "code": "x = 2", "run_after": true, "hold": true })));
        started.recv_timeout(Duration::from_secs(5)).expect("the held call started");
        let (done_tx, done) = mpsc::channel();
        scope.spawn(move || {
            let other_notebook = s.call("8", "edit_cell", json!({ "notebook_id": b, "cell_id": X, "code": "x = 3" }));
            let other_cell = s.call("7", "edit_cell", json!({ "notebook_id": a, "cell_id": Y, "code": "y = 2" }));
            done_tx.send((other_notebook, other_cell)).unwrap();
        });
        let while_held = done.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        let (other_notebook, other_cell) = while_held.expect("edits while another waits");
        assert!(other_notebook.is_ok() && other_cell.is_ok());
        assert!(held.join().unwrap().is_ok());
    });
    let (first, _) = s.notebooks.subscribe().unwrap();
    let first: Value = serde_json::from_str(&first).unwrap();
    for (notebook, edited, before) in [(a, X, "x = 1"), (a, Y, "y = 1"), (b, X, "x = 1")] {
        let state = cell(&first, notebook, edited);
        assert_eq!((&state["author"], &state["before"]), (&json!("agent"), &json!(before)), "{notebook} {edited}");
    }
}

#[test]
fn a_failed_edit_is_nobodys() {
    let s = setup();
    s.engine.open(NB, "/n/a.jl", &[(Y, "y = 1", &[])]);
    s.tools(|_, _, _| Err("stale_read".into()));
    assert_eq!(s.call("7", "edit_cell", json!({ "notebook_id": NB, "cell_id": Y, "code": "y = 2" })), Err("stale_read".into()));
    s.engine.with(NB, |nb| nb.cells[0].code = "y = 3".into());
    let (first, _) = s.notebooks.subscribe().unwrap();
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(cell(&first, NB, Y)["author"], Value::Null);
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
    s.tools(move |notebooks, tool, args| match tool {
        "open_notebook" | "new_notebook" => {
            let path = args["path"].as_str().unwrap().to_owned();
            let nid = id(notebooks.len() + 1);
            notebooks.push(FakeNotebook { id: nid.clone(), path: path.clone(), cells: Vec::new(), pending: Vec::new(), safe_preview: false });
            Ok(json!({ "notebook_id": nid, "path": path }))
        }
        _ => Ok(json!({})),
    });
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
    assert_eq!(refused(s.call("a", "fold_cell", json!({ "notebook_id": first_id, "cell_id": X, "folded": true }))), None);
    assert_eq!(refused(s.call("a", "run_all_cells", json!({ "notebook_id": "not-a-uuid" }))), None, "Julia says what's wrong");

    // A binding set by the app is respected, and clearing it lifts the limit.
    s.notebooks.bind("b", second_nb);
    assert_eq!(refused(s.call("b", "open_notebook", json!({ "path": first_nb }))), one);
    assert_eq!(refused(s.call("b", "run_all_cells", json!({ "notebook_id": first_id }))), one);
    assert_eq!(refused(s.call("b", "get_cell_order", json!({ "notebook_id": first_id }))), None);
    s.notebooks.bind("b", "");
    assert_eq!(s.notebooks.bound("b"), None);
    assert_eq!(refused(s.call("b", "run_all_cells", json!({ "notebook_id": first_id }))), None);

    // A "New notebook" session: the notebook it creates becomes its notebook.
    s.call("c", "new_notebook", json!({ "path": &paths[2] })).unwrap();
    assert_eq!(s.notebooks.bound("c").as_ref(), Some(&paths[2]));
    assert_eq!(refused(s.call("c", "new_notebook", json!({ "path": &paths[3] }))), one);
    assert_eq!(refused(s.call("c", "open_notebook", json!({ "path": first_nb }))), one);
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
        s.engine.open(&id(i), path, &[(X, "x = 1", &[])]);
        s.notebooks.notified(&json!({ "method": "notebook_opened", "params": { "notebook_id": id(i), "path": path } }));
    }
    s.engine.with(&id(0), |nb| nb.safe_preview = true);
    s.tools(|_, _, _| Ok(json!({})));
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
    s.tools(editing_tools(NB));
    s.engine.open(NB, &paths[0], &[(X, "x = 1", &[])]);
    s.call("", "edit_cell", json!({ "notebook_id": NB, "cell_id": X, "code": "x = 2" })).unwrap();
    s.call("", "keep_notebook_alive", json!({ "notebook_id": NB, "keep": true })).unwrap();
    let held = || s.notebooks.state.lock().unwrap().notebooks.get(NB).map(|nb| (nb.authors.len(), nb.befores.len(), nb.kept_alive, nb.last_active.is_some()));
    assert_eq!(held(), Some((1, 1, true, true)));

    // Restarting in place (leaving safe preview) keeps it in the session, and its state.
    s.notebooks.notified(&json!({ "method": "cell_state", "params": { "notebook_id": NB, "cells": [] } }));
    s.notebooks.publish();
    assert_eq!(held(), Some((1, 1, true, true)));

    // Pluto shutting it down (the page's own button).
    s.engine.notebooks.lock().unwrap().clear();
    s.notebooks.notified(&json!({ "method": "notebook_shut_down", "params": { "notebook_id": NB } }));
    assert_eq!(held(), None);

    // Stopped by the app, or gone by the time the core looks.
    s.engine.open(NB, &paths[1], &[(X, "x = 1", &[])]);
    s.call("", "keep_notebook_alive", json!({ "notebook_id": NB, "keep": true })).unwrap();
    s.notebooks.stop_notebook(&paths[1]).unwrap();
    assert_eq!(held(), None);
    s.engine.open(NB, &paths[2], &[(X, "x = 1", &[])]);
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
    let not_found = |shown: &str| Err(format!("KeyError: key \"notebook_not_found::No notebook with id '{shown}' in the current session\" not found"));
    assert_eq!(keep(json!(missing), json!(true)), not_found(missing));
    assert_eq!(keep(json!(123), json!(true)), not_found("123"));
    assert_eq!(s.notebooks.keep_alive(&json!({ "keep": true })), Err("ArgumentError: invalid_notebook_id::Invalid notebook ID: ''".into()));
    assert_eq!(keep(json!(NB), json!("yes")), Err("ArgumentError: invalid_keep::keep must be true or false".into()));
    assert_eq!(keep(json!(NB.to_uppercase()), json!(false)), Ok(json!({ "notebook_id": NB, "kept_alive": false })));
    let text = |raw: &str| crate::mcp::tool_error(raw)["content"][0]["text"].as_str().unwrap().to_owned();
    assert_eq!(
        text(&keep(json!(missing), json!(true)).unwrap_err()),
        format!(r#"{{"error":"key \"notebook_not_found","message":"No notebook with id '{missing}' in the current session\" not found"}}"#)
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
