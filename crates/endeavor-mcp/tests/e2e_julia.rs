//! The runtime end to end, with real Julia: the helper and core started as the
//! app starts This Mac's, the agent's MCP calls over `POST /mcp` with its
//! session headers, the app's `/endeavor/call`s, Pluto's page and WebSocket as
//! a browser reaches them, and the helper's file requests.
//!
//! Julia takes a while to start, so one test starts it once and walks through
//! the steps in order. It's ignored by default:
//!
//!     cargo test -p endeavor-mcp --test e2e_julia -- --ignored --nocapture
//!
//! Julia is `ENDEAVOR_E2E_JULIA` if set, else the app's own under
//! ~/Library/Application Support/endeavor/julia-*, else `julia` on the PATH.
//! With the app's Julia, the app's depot supplies the packages, read-only
//! behind a depot of the test's own in the target folder, which keeps what
//! Julia compiles between runs.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::find_julia;
use common::helper::Helper;
use serde_json::{Value, json};
use wire::files::{Reply, Request};
use wire::{ToApp, ToHelper};

/// Sessions as the agent's MCP config names them: one on This Mac, one on a server.
const MAC: &[(&str, &str)] = &[("X-Endeavor-Session", "1")];
const SERVER: &[(&str, &str)] = &[("X-Endeavor-Session", "2"), ("X-Endeavor-Host", "lab")];
const THIRD: &[(&str, &str)] = &[("X-Endeavor-Session", "3")];
/// A session that names its client, which joins session 1's notebook.
const JOINER: &[(&str, &str)] = &[("X-Endeavor-Session", "4"), ("X-Endeavor-Client", "Claude Code on a-laptop")];

/// A folder of the test's own, emptied.
fn fresh(path: PathBuf) -> PathBuf {
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

/// End the runtime recorded in `state`, if one runs: the core, Julia and its
/// notebook workers are one process group.
fn end_runtime(state: &Path) {
    let Ok(text) = std::fs::read_to_string(state.join("runtime.json")) else { return };
    if let Some(pid) = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["pid"].as_i64()).filter(|&p| p > 1) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    }
}

/// This Mac's runtime as the app has it: the helper, and the bridge's token.
struct Runtime {
    helper: Helper,
    token: String,
    next_id: u64,
    state: PathBuf,
}

impl Drop for Runtime {
    /// A failed step leaves no Julia behind.
    fn drop(&mut self) {
        end_runtime(&self.state);
    }
}

impl Runtime {
    /// Ask for the runtime and wait until it's ready, however long Julia takes.
    fn start(&mut self) {
        self.helper.request_start(None, true);
        let mut log = Vec::new();
        loop {
            match self.helper.next_within(Duration::from_secs(900)) {
                ToApp::FoundJulia { .. } => {}
                ToApp::Progress { line } => log.push(line),
                ToApp::Ready { token, .. } => return self.token = token,
                other => panic!("expected Ready, got {other:?}; the log:\n{}", log.join("\n")),
            }
        }
    }

    /// Stop the runtime, as the app's Restart Julia does first.
    fn stop(&self) {
        let stop = self.helper.request_stop();
        loop {
            match self.helper.next_within(Duration::from_secs(60)) {
                ToApp::Stopped { id } if id == stop => return,
                ToApp::Progress { .. } => {}
                other => panic!("expected Stopped, got {other:?}"),
            }
        }
    }

    /// POST `body` to `path` on the runtime's port, relayed through the helper
    /// as the app's and the agent's connections are: the status line and the body.
    fn post(&self, path: &str, headers: &[(&str, &str)], body: &str) -> (String, String) {
        let mut socket = self.helper.connect();
        socket.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
        let extra: String = headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
        write!(
            socket,
            "POST {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            self.token,
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        (head.lines().next().unwrap_or_default().to_owned(), body.to_owned())
    }

    /// GET `target` on the runtime's port with `headers` and no token, as a
    /// browser does: the status line, the whole head, and the body.
    fn get(&self, target: &str, headers: &str) -> (String, String, String) {
        let mut socket = self.helper.connect();
        socket.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        write!(socket, "GET {target} HTTP/1.0\r\nHost: 127.0.0.1\r\n{headers}\r\n").unwrap();
        let mut reply = Vec::new();
        socket.read_to_end(&mut reply).unwrap();
        let reply = String::from_utf8_lossy(&reply).into_owned();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        (head.lines().next().unwrap_or_default().to_owned(), head.to_owned(), body.to_owned())
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// One MCP request from a session: its JSON-RPC reply.
    fn mcp(&mut self, caller: &[(&str, &str)], method: &str, params: Value) -> Value {
        let id = self.id();
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (status, body) = self.post("/mcp", caller, &message.to_string());
        assert_eq!(status, "HTTP/1.1 200 OK", "{method}: {body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["id"], id);
        reply
    }

    /// A session's tool call: the tool's JSON answer, or its error.
    fn tool(&mut self, caller: &[(&str, &str)], name: &str, arguments: Value) -> Result<Value, Value> {
        let reply = self.mcp(caller, "tools/call", json!({ "name": name, "arguments": arguments }));
        let result = &reply["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {reply}"));
        let answer = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        if result["isError"] == true { Err(answer) } else { Ok(answer) }
    }

    fn ok(&mut self, caller: &[(&str, &str)], name: &str, arguments: Value) -> Value {
        self.tool(caller, name, arguments.clone()).unwrap_or_else(|e| panic!("{name}({arguments}) failed: {e}"))
    }

    /// One of the app's `/endeavor/call`s: its result.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.id();
        let (status, body) = self.post("/endeavor/call", &[], &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string());
        assert_eq!(status, "HTTP/1.1 200 OK", "{method}: {body}");
        let reply: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("{method}: {body}"));
        match reply.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(reply["result"].clone()),
        }
    }

    /// A notebook tool the app calls itself, through `/endeavor/call`: its JSON answer.
    fn app_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.call("tools/call", json!({ "name": name, "arguments": arguments })).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(result["isError"], false, "{name}: {result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// `list_notebooks` as a session sees it.
    fn notebooks(&mut self, caller: &[(&str, &str)]) -> Vec<Value> {
        let listed = self.ok(caller, "list_notebooks", json!({}));
        listed.as_array().unwrap_or_else(|| panic!("list_notebooks: {listed}")).clone()
    }

    /// A file request to the helper, as the app sends an upload.
    fn files(&self, id: u32, request: Request) -> Reply {
        self.helper.send(ToHelper::Files { id, request });
        loop {
            match self.helper.next() {
                ToApp::Files { id: got, reply } if got == id => return reply,
                ToApp::Progress { .. } => {}
                other => panic!("expected Files {id}, got {other:?}"),
            }
        }
    }
}

/// A step's name, and how long it took, as the run goes.
fn step<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    eprintln!("── {name}");
    let result = f();
    eprintln!("   {:.1}s", started.elapsed().as_secs_f64());
    result
}

fn modified(runtime: &mut Runtime, path: &str) -> f64 {
    runtime.call("endeavor/file_info", json!({ "path": path })).unwrap()["modified"].as_f64().unwrap()
}

#[test]
#[ignore = "starts real Julia, about 40 s: cargo test -p endeavor-mcp --test e2e_julia -- --ignored"]
fn the_runtime_end_to_end() {
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return;
    };
    let started = Instant::now();
    eprintln!("julia: {}", julia.display());
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-julia");
    let depot = work.join("depot");
    std::fs::create_dir_all(&depot).unwrap();
    // Ours first, where Julia writes; the app's behind it, read-only; the defaults last.
    let depot_path = match &app {
        Some(app) => format!("{}:{}:", depot.display(), app.join("depot").display()),
        None => format!("{}:", depot.display()),
    };
    end_runtime(&work.join("state"));
    let state = fresh(work.join("state"));
    let mac = fresh(work.join("mac"));
    let lab = fresh(work.join("lab"));
    let runtime_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").canonicalize().unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command
        .args(["connect", "--state-dir"])
        .arg(&state)
        .arg("--julia")
        .arg(&julia)
        .arg("--runtime")
        .arg(&runtime_dir)
        .args(["--depot", &depot_path, "--any-node", "--quit-with-client"])
        .env("ENDEAVOR_IDLE_CHECK_SECS", "1");
    let helper = Helper::spawn(command);
    let ToApp::Hello { uploads, .. } = helper.hello() else { unreachable!() };
    assert!(uploads, "the helper takes uploads");
    let mut rt = Runtime { helper, token: String::new(), next_id: 0, state: state.clone() };
    step("start Julia", || rt.start());

    step("MCP handshake", || {
        let init = rt.mcp(MAC, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        assert_eq!(init["result"]["serverInfo"]["name"], "endeavor-runtime");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        let (status, body) = rt.post("/mcp", MAC, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!((status.as_str(), body.as_str()), ("HTTP/1.1 202 Accepted", ""));
        let mut names = |caller| {
            let listed = rt.mcp(caller, "tools/list", json!({}));
            listed["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
        };
        let (on_mac, on_server) = (names(MAC), names(SERVER));
        for tool in ["list_notebooks", "new_notebook", "add_cell", "edit_cell", "execute_cell", "read_cell", "allow_execution"] {
            assert!(on_mac.contains(&tool.to_owned()), "tools/list has {tool}: {on_mac:?}");
        }
        assert!(!on_mac.contains(&"run_shell".to_owned()) && on_server.contains(&"run_shell".to_owned()), "host tools only for a server session");
    });

    for (owner, folder) in [("1", &mac), ("2", &lab), ("3", &mac)] {
        rt.call("endeavor/set_session_folder", json!({ "owner": owner, "folder": folder })).unwrap();
        rt.call("endeavor/set_policy", json!({ "owner": owner, "policy": "ask" })).unwrap();
    }

    let (notebook, path, cell) = step("new notebook, add and edit a cell, run it, read its output", || {
        let created = rt.ok(MAC, "new_notebook", json!({}));
        let notebook = created["notebook_id"].as_str().unwrap_or_else(|| panic!("new_notebook: {created}")).to_owned();
        let path = created["path"].as_str().unwrap().to_owned();
        assert!(path.starts_with(mac.to_str().unwrap()) && path.ends_with(".jl"), "made in the session's folder: {path}");
        let order = rt.ok(MAC, "get_cell_order", json!({ "notebook_id": notebook }));
        let last = order["cell_ids"].as_array().and_then(|ids| ids.last()).unwrap_or_else(|| panic!("get_cell_order: {order}")).clone();
        let added = rt.ok(MAC, "add_cell", json!({ "notebook_id": notebook, "code": "x = 20", "after_cell_id": last }));
        let cell = added["cell_id"].as_str().unwrap_or_else(|| panic!("add_cell: {added}")).to_owned();
        rt.ok(MAC, "edit_cell", json!({ "notebook_id": notebook, "cell_id": cell, "code": "x = 21 * 2" }));
        rt.ok(MAC, "execute_cell", json!({ "notebook_id": notebook, "cell_id": cell, "wait_for_completion": true }));
        let read = rt.ok(MAC, "read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
        assert_eq!((&read["code"], &read["output"], &read["errored"]), (&json!("x = 21 * 2"), &json!("42"), &json!(false)), "{read}");
        let added = rt.ok(MAC, "add_cell", json!({ "notebook_id": notebook, "code": "[1.5, 2.5]", "after_cell_id": cell }));
        let rich = added["cell_id"].as_str().unwrap().to_owned();
        let ran = rt.ok(MAC, "execute_cell", json!({ "notebook_id": notebook, "cell_id": rich, "wait_for_completion": true }));
        assert_eq!(ran["outputs"]["changed"][0]["output_text"], json!("2-element Vector{Float64}:\n 1.5\n 2.5"), "a run's receipt: {ran}");
        let read = rt.ok(MAC, "read_cell", json!({ "notebook_id": notebook, "cell_id": rich }));
        assert_eq!(read["output_text"], json!("2-element Vector{Float64}:\n 1.5\n 2.5"), "a tree output read as text: {read}");
        (notebook, path, cell)
    });

    step("Pluto's page and WebSocket, as a browser reaches them", || {
        let token = rt.token.clone();
        let (status, head, _) = rt.get(&format!("/edit?id={notebook}&token={token}"), "");
        assert_eq!(status, "HTTP/1.1 303 See Other", "{head}");
        assert!(head.contains(&format!("\r\nLocation: /edit?id={notebook}\r\n")), "{head}");
        let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}"));
        let cookie = format!("Cookie: {}\r\n", set.split(';').next().unwrap());
        let (status, head, page) = rt.get(&format!("/edit?id={notebook}"), &format!("{cookie}Sec-Fetch-Site: none\r\n"));
        assert_eq!(status, "HTTP/1.1 200 OK", "{head}");
        assert!(page.contains("<html") && page.contains("Pluto"), "Pluto's page: {}", &page[..page.len().min(300)]);
        assert!(!head.to_ascii_lowercase().contains("set-cookie: secret="), "Pluto's secret stays in the core: {head}");
        assert!(rt.get("/", "").0.starts_with("HTTP/1.1 401"), "nothing without the token or the cookie");
        assert!(rt.get("/", &format!("{cookie}Origin: http://127.0.0.1:1\r\n")).0.starts_with("HTTP/1.1 403"), "another page's request");

        let mut socket = rt.helper.connect();
        socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(
            socket,
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://127.0.0.1\r\n{cookie}Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        )
        .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
        // A masked ping with the payload "hi"; Pluto's server answers with a pong.
        let mask = [1u8, 2, 3, 4];
        let mut ping = vec![0x89, 0x80 | 2];
        ping.extend(mask);
        ping.extend(b"hi".iter().zip(mask).map(|(b, m)| b ^ m));
        socket.write_all(&ping).unwrap();
        let mut pong = [0u8; 4];
        socket.read_exact(&mut pong).unwrap();
        assert_eq!(pong, [0x8A, 2, b'h', b'i'], "a pong through the core, both ways");
    });

    step("list_notebooks and one notebook per session", || {
        let created = rt.ok(SERVER, "new_notebook", json!({}));
        let other = created["notebook_id"].as_str().unwrap().to_owned();
        let this_session = |rt: &mut Runtime, caller| {
            let list = rt.notebooks(caller);
            let mut marked: Vec<(String, bool)> = list.iter().map(|nb| (nb["notebook_id"].as_str().unwrap().to_owned(), nb["this_session"] == true)).collect();
            marked.sort();
            marked
        };
        let mut expected = vec![(notebook.clone(), true), (other.clone(), false)];
        expected.sort();
        assert_eq!(this_session(&mut rt, MAC), expected, "session 1 sees its own notebook as this_session");
        let mut expected = vec![(notebook.clone(), false), (other.clone(), true)];
        expected.sort();
        assert_eq!(this_session(&mut rt, SERVER), expected, "session 2 sees its own notebook as this_session");

        let second = rt.tool(MAC, "new_notebook", json!({})).expect_err("a second notebook is refused");
        assert_eq!(second["error"], "one_notebook", "{second}");
        let edit = rt.tool(SERVER, "edit_cell", json!({ "notebook_id": notebook, "cell_id": cell, "code": "x = 0" })).expect_err("another session's notebook can't be changed");
        assert_eq!(edit["error"], "one_notebook", "{edit}");
        rt.ok(SERVER, "read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
        other
    });

    step("a second session opens the first one's notebook by path and joins it", || {
        let joined = rt.ok(JOINER, "open_notebook", json!({ "path": path, "run_notebook": true }));
        assert_eq!((&joined["notebook_id"], &joined["path"], &joined["already_open"], &joined["ran"]), (&json!(notebook), &json!(path), &json!(true), &json!(false)), "{joined}");
        assert_eq!((&joined["execution_allowed"], &joined["process_status"]), (&json!(true), &json!("ready")), "as the notebook was, not run again: {joined}");
        let seen_by = |rt: &mut Runtime, caller| {
            let list = rt.notebooks(caller);
            let nb = list.iter().find(|nb| nb["notebook_id"] == notebook.as_str()).unwrap().clone();
            (nb["this_session"].clone(), nb["other_sessions"].clone())
        };
        let (mine, others) = seen_by(&mut rt, MAC);
        assert_eq!(mine, json!(true), "still session 1's");
        assert_eq!(others.as_array().unwrap().len(), 1, "{others}");
        assert_eq!(others[0]["client"], json!("Claude Code on a-laptop"));
        assert!(others[0]["active_seconds_ago"].as_u64().is_some_and(|s| s < 60), "{others}");
        let (mine, others) = seen_by(&mut rt, JOINER);
        assert_eq!(mine, json!(true), "and now session 4's");
        assert_eq!(others.as_array().unwrap().len(), 1, "{others}");
        assert_eq!(others[0]["client"], json!(null), "session 1 named no client");
        assert!(others[0]["active_seconds_ago"].as_u64().is_some_and(|s| s < 60), "{others}");
        let status = rt.ok(JOINER, "pluto_session_status", json!({}));
        let own = status["notebooks"].as_array().unwrap().iter().find(|nb| nb["notebook_id"] == notebook.as_str()).unwrap().clone();
        assert_eq!(own["other_sessions"].as_array().unwrap().len(), 1, "{status}");
        let list = rt.notebooks(JOINER);
        let elsewhere = list.iter().find(|nb| nb["notebook_id"] != notebook.as_str()).unwrap()["path"].as_str().unwrap().to_owned();
        let again = rt.tool(JOINER, "open_notebook", json!({ "path": elsewhere })).expect_err("it still works on one notebook");
        assert_eq!(again["error"], "one_notebook", "{again}");
        // The app's own call is no session's and gets the notebook as it is.
        let app = rt.app_tool("open_notebook", json!({ "path": path }));
        assert_eq!((&app["notebook_id"], &app["already_open"]), (&json!(notebook), &json!(true)), "{app}");
    });

    step("run policy: what an asked run would run, and plan mode", || {
        let added = rt.ok(MAC, "add_cell", json!({ "notebook_id": notebook, "code": "y = x + 1", "after_cell_id": cell }));
        let dependent = added["cell_id"].as_str().unwrap().to_owned();
        let preview = rt.call("endeavor/run_preview", json!({ "tool": "execute_cell", "arguments": { "notebook_id": notebook, "cell_id": cell } })).unwrap();
        assert_eq!((preview["count"].clone(), preview["dependents"].clone()), (json!(1), json!(1)), "run x, and y re-runs: {preview}");

        rt.call("endeavor/set_policy", json!({ "owner": "1", "policy": "plan" })).unwrap();
        let refused = rt.tool(MAC, "edit_cell", json!({ "notebook_id": notebook, "cell_id": dependent, "code": "y = 0" })).expect_err("plan mode refuses writes");
        assert_eq!(refused["error"], "plan_mode", "{refused}");
        let run = rt.tool(MAC, "execute_cell", json!({ "notebook_id": notebook, "cell_id": dependent })).expect_err("plan mode refuses runs");
        assert_eq!(run["error"], "plan_mode", "{run}");
        rt.ok(MAC, "read_cell", json!({ "notebook_id": notebook, "cell_id": dependent }));
        rt.call("endeavor/set_policy", json!({ "owner": "1", "policy": "ask" })).unwrap();
        rt.ok(MAC, "execute_cell", json!({ "notebook_id": notebook, "cell_id": dependent, "wait_for_completion": true }));
    });

    step("uploads: a same-name file is reused or numbered", || {
        let folder = mac.to_str().unwrap().to_owned();
        let sources = fresh(work.join("sources"));
        let send = |rt: &Runtime, id: u32, name: &str, bytes: &[u8]| {
            let source = sources.join(format!("{id}-{name}"));
            std::fs::write(&source, bytes).unwrap();
            let sha256 = wire::files::sha256_file(&source).unwrap();
            let placed = rt.files(id, Request::Place { folder: folder.clone(), name: name.into(), size: bytes.len() as u64, sha256 });
            let Reply::Place { path, have } = placed else { panic!("Place: {placed:?}") };
            if !have {
                let written = rt.files(id + 1, Request::Write { folder: folder.clone(), path: path.clone(), offset: 0, bytes: bytes.to_vec(), last: true });
                assert_eq!(written, Reply::Written);
            }
            (path, have)
        };
        assert_eq!(send(&rt, 10, "decay.csv", b"t,y\n0,1\n"), ("data/decay.csv".into(), false));
        assert_eq!(send(&rt, 20, "decay.csv", b"t,y\n0,1\n"), ("data/decay.csv".into(), true), "the same file is reused");
        assert_eq!(send(&rt, 30, "decay.csv", b"t,y\n0,2\n"), ("data/decay (2).csv".into(), false), "a different one gets a number");
        assert_eq!(send(&rt, 40, "decay.csv", b"t,y\n0,2\n"), ("data/decay (2).csv".into(), true), "and is reused in turn");
        assert_eq!(std::fs::read_to_string(mac.join("data/decay.csv")).unwrap(), "t,y\n0,1\n");
        assert_eq!(std::fs::read_to_string(mac.join("data/decay (2).csv")).unwrap(), "t,y\n0,2\n");
    });

    let other_path = step("restart: unchanged comes back running, changed in safe preview", || {
        let list = rt.notebooks(MAC);
        let other_path = list.iter().find(|nb| nb["notebook_id"] != notebook.as_str()).unwrap()["path"].as_str().unwrap().to_owned();
        // As restart_local: the allowed notebooks and their files' times, then Stop and Start.
        let allowed: Vec<(String, f64)> = list
            .iter()
            .filter(|nb| nb["execution_allowed"] == true)
            .map(|nb| nb["path"].as_str().unwrap().to_owned())
            .map(|p| (p.clone(), modified(&mut rt, &p)))
            .collect();
        assert_eq!(allowed.len(), 2, "both notebooks run: {list:?}");
        rt.stop();
        let text = std::fs::read_to_string(&other_path).unwrap();
        std::fs::write(&other_path, format!("{text}\n")).unwrap();
        rt.start();
        // As reopen_notebooks: each runs again if it ran and its file is as it was.
        let mut reopened = Vec::new();
        for (p, was) in &allowed {
            let run = modified(&mut rt, p) == *was;
            let opened = rt.app_tool("open_notebook", json!({ "path": p, "run_notebook": run }));
            reopened.push((p.clone(), opened["execution_allowed"].clone()));
        }
        reopened.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected = vec![(path.clone(), json!(true)), (other_path.clone(), json!(false))];
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(reopened, expected, "the unchanged notebook runs; the changed one opens in safe preview");
        other_path
    });

    let previewed = step("a notebook in safe preview runs once allowed", || {
        let list = rt.app_tool("list_notebooks", json!({}));
        let previewed = list.as_array().unwrap().iter().find(|nb| nb["path"] == other_path.as_str()).unwrap()["notebook_id"].as_str().unwrap().to_owned();
        rt.call("endeavor/set_notebook", json!({ "owner": "2", "notebook": other_path })).unwrap();
        let order = rt.ok(SERVER, "get_cell_order", json!({ "notebook_id": previewed }));
        let first = order["cell_ids"][0].clone();
        let run = json!({ "notebook_id": previewed, "cell_id": first, "wait_for_completion": true });
        let blocked = rt.ok(SERVER, "execute_cell", run.clone());
        assert_eq!(blocked["execution"]["status"], "blocked", "nothing runs in safe preview: {blocked}");
        let allowed = rt.ok(SERVER, "allow_execution", json!({ "notebook_id": previewed }));
        assert_eq!(allowed["execution_allowed"], true, "{allowed}");
        let ran = rt.ok(SERVER, "execute_cell", run);
        assert_eq!(ran["execution"]["status"], "completed", "{ran}");
        previewed
    });

    step("out of safe preview without a run, a cell's never-run upstream cells run with it", || {
        let ids = ["5a1e0001-bbeb-11f1-8e5f-a5a8320edb60", "5a1e0002-bbeb-11f1-8e5f-a5a8320edb60", "5a1e0003-bbeb-11f1-8e5f-a5a8320edb60", "5a1e0004-bbeb-11f1-8e5f-a5a8320edb60"];
        let codes = ["a = 5", "b = a + 1", "c = b * 3", "e = c * 2"];
        let mut text = String::from("### A Pluto.jl notebook ###\n# v1.0.3\n\nusing Markdown\nusing InteractiveUtils\n\n");
        for (id, code) in ids.iter().zip(codes) {
            text.push_str(&format!("# ╔═╡ {id}\n{code}\n\n"));
        }
        text.push_str("# ╔═╡ Cell order:\n");
        for id in ids {
            text.push_str(&format!("# ╠═{id}\n"));
        }
        let cards = mac.join("cards.jl");
        std::fs::write(&cards, text).unwrap();
        let cards = cards.to_str().unwrap().to_owned();
        let opened = rt.app_tool("open_notebook", json!({ "path": cards, "run_notebook": false }));
        let id = opened["notebook_id"].as_str().unwrap().to_owned();
        rt.call("endeavor/set_notebook", json!({ "owner": "3", "notebook": cards })).unwrap();

        let preview = rt.call("endeavor/run_preview", json!({ "tool": "allow_execution", "arguments": { "notebook_id": id, "run_notebook": false } })).unwrap();
        assert_eq!((&preview["all"], &preview["count"]), (&json!(false), &json!(4)), "the card counts the notebook's cells: {preview}");

        rt.ok(THIRD, "allow_execution", json!({ "notebook_id": id, "run_notebook": false }));
        let run = json!({ "notebook_id": id, "cell_id": ids[1], "wait_for_completion": true });
        let preview = rt.call("endeavor/run_preview", json!({ "tool": "execute_cell", "arguments": run })).unwrap();
        assert_eq!((&preview["count"], &preview["needed_ids"], &preview["dependent_ids"]), (&json!(1), &json!([ids[0]]), &json!([ids[2], ids[3]])), "b needs a: {preview}");
        let ran = rt.ok(THIRD, "execute_cell", run);
        assert_eq!(ran["execution"]["status"], "completed", "{ran}");
        let read = |rt: &mut Runtime, cell: &str| {
            let read = rt.ok(THIRD, "read_cell", json!({ "notebook_id": id, "cell_id": cell }));
            (read["output"].clone(), read["errored"].clone())
        };
        assert_eq!(read(&mut rt, ids[0]), (json!("5"), json!(false)));
        assert_eq!(read(&mut rt, ids[1]), (json!("6"), json!(false)), "b ran after a");
        let preview = rt.call("endeavor/run_preview", json!({ "tool": "execute_cell", "arguments": { "notebook_id": id, "cell_id": ids[1] } })).unwrap();
        assert_eq!(preview["needed_ids"], json!([]), "a has run now: {preview}");
        rt.call("endeavor/stop_notebook", json!({ "path": cards })).unwrap();
    });

    step("a notebook's own Julia ending by itself ends the run waiting for it", || {
        let order = rt.ok(SERVER, "get_cell_order", json!({ "notebook_id": previewed }));
        let last = order["cell_ids"].as_array().unwrap().last().unwrap().clone();
        rt.ok(SERVER, "read_notebook_code", json!({ "notebook_id": previewed }));
        let added = rt.ok(SERVER, "add_cell", json!({ "notebook_id": previewed, "code": "worker = getpid()", "after_cell_id": last }));
        let pid_cell = added["cell_id"].clone();
        rt.ok(SERVER, "execute_cell", json!({ "notebook_id": previewed, "cell_id": pid_cell, "wait_for_completion": true }));
        let worker: i32 = rt.ok(SERVER, "read_cell", json!({ "notebook_id": previewed, "cell_id": pid_cell }))["output"].as_str().unwrap().parse().unwrap();
        let added = rt.ok(SERVER, "add_cell", json!({ "notebook_id": previewed, "code": "rates = (sleep(600); 1)", "after_cell_id": pid_cell }));
        let rates = added["cell_id"].as_str().unwrap().to_owned();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            // SAFETY: plain syscall.
            unsafe { libc::kill(worker, libc::SIGKILL) };
        });
        let ended = rt.tool(SERVER, "execute_cell", json!({ "notebook_id": previewed, "cell_id": rates, "wait_for_completion": true }));
        killer.join().unwrap();
        assert_eq!(
            ended,
            Err(json!({
                "error": "process_exited",
                "message": "Julia stopped unexpectedly while running `rates`. The notebook file is saved; its outputs are gone until the cells run again."
            }))
        );
        let listed = rt.notebooks(SERVER).into_iter().find(|nb| nb["notebook_id"] == previewed.as_str()).unwrap();
        assert_eq!((&listed["exited"], &listed["running"], &listed["execution_allowed"]), (&json!({ "running": [rates] }), &json!([]), &json!(false)), "{listed}");
    });

    step("idle stop with a short limit", || {
        let mut events = rt.helper.connect();
        events.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        write!(events, "GET /endeavor/events HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n\r\n", rt.token).unwrap();
        // About two seconds.
        rt.call("endeavor/set_idle_limit", json!({ "hours": 0.0005 })).unwrap();
        let mut seen = String::new();
        let mut buffer = [0u8; 8192];
        let deadline = Instant::now() + Duration::from_secs(60);
        while !seen.contains(r#""idle_stopped":[{"#) {
            assert!(Instant::now() < deadline, "no idle stop within a minute; events:\n{seen}");
            let n = events.read(&mut buffer).unwrap();
            assert!(n > 0, "the events stream ended:\n{seen}");
            seen.push_str(&String::from_utf8_lossy(&buffer[..n]));
        }
        let last = seen.lines().rev().find(|l| l.starts_with("data: ") && l.contains("idle_stopped")).unwrap();
        let state: Value = serde_json::from_str(&last["data: ".len()..]).unwrap();
        let stopped: Vec<&str> = state["idle_stopped"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
        assert!(stopped.contains(&path.as_str()), "{path} stopped for being idle: {state}");
        assert_eq!(state["idle_stopped"][0]["hours"], 0, "{state}");
    });

    rt.stop();
    eprintln!("all steps: {:.0}s", started.elapsed().as_secs_f64());
}
