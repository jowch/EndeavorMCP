#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn fresh(path: PathBuf) -> PathBuf {
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn recorded_pid(state: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(state.join("runtime.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["pid"].as_i64().map(|p| p as i32).filter(|&p| p > 1)
}

struct Cleanup {
    state: PathBuf,
    children: Vec<Child>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let log = self.state.join("runtime.log");
            eprintln!("── the runtime's log, {}:\n{}", log.display(), std::fs::read_to_string(&log).unwrap_or_else(|e| format!("(not read: {e})")));
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(pid) = recorded_pid(&self.state) {
            // SAFETY: plain syscall; the core leads its process group.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
    }
}

fn lines(stream: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn post(port: u16, token: &str, session: Option<&str>, message: &Value) -> (String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(600))).unwrap();
    let body = message.to_string();
    let session = session.map_or(String::new(), |id| format!("Mcp-Session-Id: {id}\r\n"));
    write!(
        socket,
        "POST /mcp HTTP/1.0\r\nHost: localhost:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nX-Endeavor-Consent: front\r\n{session}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.to_owned(), body.to_owned())
}

fn get(port: u16, target: &str, headers: &str) -> (String, String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(socket, "GET {target} HTTP/1.0\r\nHost: localhost:{port}\r\n{headers}\r\n").unwrap();
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply).unwrap();
    let reply = String::from_utf8_lossy(&reply).into_owned();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.lines().next().unwrap_or_default().to_owned(), head.to_owned(), body.to_owned())
}

struct Agent {
    port: u16,
    token: String,
    id: u64,
    session: Option<String>,
}

impl Agent {
    fn mcp(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let (head, body) = post(self.port, &self.token, self.session.as_deref(), &json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params }));
        assert_eq!(head.lines().next(), Some("HTTP/1.1 200 OK"), "{method}: {body}");
        if method == "initialize" {
            self.session = head.lines().find_map(|line| line.strip_prefix("Mcp-Session-Id: ")).map(str::to_owned);
        }
        serde_json::from_str(&body).unwrap()
    }

    fn call(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let reply = self.mcp("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {reply}"));
        (reply["result"]["isError"] == true, serde_json::from_str(text).unwrap_or_else(|_| json!(text)))
    }

    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let (failed, result) = self.call(name, arguments.clone());
        assert!(!failed, "{name}({arguments}): {result}");
        result
    }
}

fn step<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    eprintln!("── {name}");
    let result = f();
    eprintln!("   {:.1}s", started.elapsed().as_secs_f64());
    result
}

fn header(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap();
    text.split("\n# ///\n").next().unwrap_or_default().to_owned()
}

fn has_uv() -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    Command::new("sh").args(["-c", "command -v uv"]).stdout(Stdio::null()).status().is_ok_and(|s| s.success()) || home.join(".local/bin/uv").is_file()
}

const GROWTH: &str = r#"# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "marimo==0.25.1",
# ]
# ///

import marimo

__generated_with = "0.25.1"
app = marimo.App()


@app.cell
def _():
    x = 20
    return (x,)


@app.cell
def _(x):
    y = x + 1
    y
    return


if __name__ == "__main__":
    app.run()
"#;

const PAGE: &str = r#"
import asyncio, json, sys, websockets
async def main(url, cookie, want):
    async with websockets.connect(url, additional_headers={"Cookie": cookie}, max_size=None) as ws:
        async for raw in ws:
            m = json.loads(raw)
            if m.get("op") == "kernel-ready":
                print("main" if not m["data"].get("kiosk") else "kiosk", flush=True)
            if m.get("op") == "notebook-document-transaction" and want in raw:
                print("got", flush=True)
                return
asyncio.run(main(*sys.argv[1:]))
"#;

const PAGE_DISABLES: &str = r#"
import asyncio, json, sys, urllib.request, websockets
async def main(base, session, cookie, token, cell):
    async with websockets.connect(f"ws://{base}/marimo/ws?session_id={session}&file={sys.argv[6]}", additional_headers={"Cookie": cookie}, max_size=None) as ws:
        async for raw in ws:
            if json.loads(raw).get("op") == "kernel-ready":
                break
        change = {"type": "set-config", "cellId": cell, "column": None, "disabled": True, "hideCode": False, "expandOutput": False}
        created = {"type": "create-cell", "cellId": "Zpge", "code": "z = 1", "name": "_", "after": cell,
                   "config": {"column": None, "disabled": True, "hide_code": False, "expand_output": False}}
        request = urllib.request.Request(f"http://{base}/marimo/api/document/transaction", data=json.dumps({"changes": [change, created]}).encode(), method="POST",
                                         headers={"Cookie": cookie, "Content-Type": "application/json", "Marimo-Session-Id": session, "Marimo-Server-Token": token})
        with urllib.request.urlopen(request) as reply:
            print(reply.status, flush=True)
        await asyncio.sleep(1)
asyncio.run(main(*sys.argv[1:6]))
"#;

#[test]
#[ignore = "starts real marimo: cargo test -p endeavor-mcp --test e2e_marimo -- --ignored"]
fn a_python_notebook_through_the_runtime() {
    if !has_uv() {
        eprintln!("SKIPPED: no uv.");
        return;
    }
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-marimo");
    if let Some(pid) = recorded_pid(&work.join("state")) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    let state = fresh(work.join("state"));
    let folder = fresh(work.join("project"));
    let mut cleanup = Cleanup { state: state.clone(), children: Vec::new() };
    std::fs::write(folder.join("growth.py"), GROWTH).unwrap();
    let julia = work.join("no julia here");
    let mut serve = Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .args(["serve", "--folder", folder.to_str().unwrap(), "--state-dir"])
        .arg(&state)
        .args(["--julia", julia.to_str().unwrap(), "--depot", &format!("{}:", work.join("depot").display())])
        .env("XDG_CACHE_HOME", work.join("cache"))
        .env("XDG_STATE_HOME", work.join("state-home"))
        .env("ENDEAVOR_TEST_MARIMO_INSTALL_DELAY_SECS", "3")
        .env("ENDEAVOR_TEST_MARIMO_WAIT_SECS", "0")
        .env("XDG_CONFIG_HOME", work.join("config"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = lines(serve.stdout.take().unwrap());
    let err = lines(serve.stderr.take().unwrap());
    let printed = step("serve starts without Julia", || {
        let mut printed = Vec::new();
        loop {
            match out.recv_timeout(Duration::from_secs(120)) {
                Ok(line) if line == "Press Ctrl-C to stop Endeavor." => break printed,
                Ok(line) => printed.push(line),
                Err(_) => panic!("serve printed {printed:?}; its log:\n{}", err.try_iter().collect::<Vec<_>>().join("\n")),
            }
        }
    });
    cleanup.children.push(serve);
    let link = printed.iter().find_map(|l| l.trim().strip_prefix("http://localhost:")).filter(|l| l.contains("/?token=")).unwrap();
    let (port, pages) = link.split_once("/?token=").unwrap();
    let (port, pages): (u16, String) = (port.parse().unwrap(), pages.to_owned());
    let token = printed.iter().find_map(|l| l.trim().strip_prefix("Header: Authorization: Bearer ")).unwrap().to_owned();
    let mut agent = Agent { port, token, id: 0, session: None };
    agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));

    let path = folder.join("growth.py").display().to_string();
    let notebook = step("opening a marimo notebook starts marimo, in safe preview", || {
        let (failed, first) = agent.call("open_notebook", json!({ "path": "growth.py" }));
        assert!(!failed && first["error"] == "python_installing", "{first}");
        assert!(first["message"].as_str().unwrap_or_default().contains("Make the same call again"), "{first}");
        let began = Instant::now();
        let opened = loop {
            let (failed, opened) = agent.call("open_notebook", json!({ "path": "growth.py" }));
            assert!(!failed, "{opened}");
            if opened["error"] != "python_installing" {
                break opened;
            }
            assert!(began.elapsed() < Duration::from_secs(120), "marimo didn't start: {opened}");
            std::thread::sleep(Duration::from_millis(200));
        };
        assert_eq!((&opened["path"], &opened["execution_allowed"]), (&json!(path), &json!(false)), "{opened}");
        let id = opened["notebook_id"].as_str().unwrap().to_owned();
        let encoded = path.replace(' ', "%20");
        assert_eq!(opened["browser_url"], json!(format!("http://localhost:{port}/marimo/?file={encoded}")), "{opened}");
        let marimo = std::fs::read_to_string(state.join("marimo.json")).expect("the adapter wrote its state");
        assert!(marimo.contains("marimo_port") && !marimo.contains("token"), "the core made marimo's token: {marimo}");
        let (_, head, _) = get(port, &format!("/marimo/?token={pages}"), "");
        let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}"));
        let cookie = format!("Cookie: {}\r\nSec-Fetch-Site: none\r\n", set.split(';').next().unwrap());
        for target in [format!("/marimo/?file={encoded}"), format!("/marimo/ws?session_id=s&file={encoded}"), format!("/marimo/?file={}", folder.join("other.py").display())] {
            let (status, _, page) = get(port, &target, &cookie);
            assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
            assert!(page.contains("isn't allowed to run yet"), "{page}");
        }
        id
    });

    let order = step("read, then run in order; an edit in safe preview goes to the file and keeps its ids", || {
        let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }))["cell_ids"].clone();
        let order: Vec<String> = order.as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
        assert_eq!(order.len(), 2, "{order:?}");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0] }));
        assert_eq!(read["code"], "x = 20", "{read}");
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 41" }));
        let added = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "w = 1", "after_cell_id": order[0] }))["cell_id"].as_str().unwrap().to_owned();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("x = 41") && saved.contains("w = 1") && saved.contains("#     \"marimo==0.25.1\","), "the preview saved the edits and kept the header: {saved}");
        let preview: Value = agent.ok("get_cell_order", json!({ "notebook_id": notebook }))["cell_ids"].clone();
        assert_eq!(preview.as_array().unwrap().len(), 3, "{preview}");
        agent.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
        assert_eq!(agent.ok("get_cell_order", json!({ "notebook_id": notebook }))["cell_ids"], preview, "the ids survive going live");
        agent.ok("run_all_cells", json!({ "notebook_id": notebook, "wait_for_completion": true }));
        for (cell, out) in [(&order[1], "42"), (&added, "")] {
            let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
            assert!(read["errored"] == false && read["not_run"] != true && read["output"].as_str().unwrap_or_default().contains(out), "{read}");
        }
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "include_code": true }));
        assert_eq!(read["code"], "x = 41", "{read}");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": added, "include_code": true }));
        assert_eq!(read["code"], "w = 1", "{read}");
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": added, "code": "w = 2" }));
        assert!(std::fs::read_to_string(&path).unwrap().contains("w = 2"), "the live session knows the added cell by its preview id");
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": order[1], "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[1] }));
        assert_eq!(read["errored"], false, "{read}");
        assert!(read["output"].as_str().unwrap_or_default().contains("42"), "{read}");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("x = 41"), "marimo saved the edit: {saved}");
        order
    });

    step("an added cell runs; an error comes back as an error", || {
        let added = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "z = y * 2\nz", "after_cell_id": order[1] }));
        let c = added["cell_id"].as_str().unwrap().to_owned();
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": c, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": c }));
        assert!(read["output"].as_str().unwrap_or_default().contains("84"), "{read}");
        agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0] }));
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 50" }));
        let ran = agent.ok("submit_changes", json!({ "notebook_id": notebook, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": c }));
        assert!(read["stale"] == false && read["output"].as_str().unwrap_or_default().contains("102"), "the cell two below ran again: {read}\n{ran}");
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": c, "code": "1 / 0" }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": c, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": c }));
        assert_eq!(read["errored"], true, "{read}");
        assert!(read.to_string().contains("ZeroDivisionError"), "{read}");
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": c, "code": "z = y * 3\nz", "folded": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": c }));
        assert_eq!(read["code_folded"], true, "{read}");
        agent.ok("fold_cell", json!({ "notebook_id": notebook, "cell_id": c, "folded": false }));
        assert_eq!(agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": c }))["code_folded"], false);
        agent.ok("fold_cell", json!({ "notebook_id": notebook, "cell_id": c, "folded": true }));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("@app.cell(hide_code=True)\ndef _(y):\n    z = y * 3"), "marimo saved the fold: {saved}");
        let def = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "gone_name = 41", "after_cell_id": c }))["cell_id"].as_str().unwrap().to_owned();
        let reader = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "gone_name + 1", "after_cell_id": def }))["cell_id"].as_str().unwrap().to_owned();
        agent.ok("submit_changes", json!({ "notebook_id": notebook, "wait_for_completion": true }));
        assert_eq!(agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": reader }))["output"], "42");
        let slow = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "import time as _t\n_t.sleep(3)", "after_cell_id": reader }))["cell_id"].as_str().unwrap().to_owned();
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": slow, "wait_for_completion": false }));
        std::thread::sleep(Duration::from_millis(500));
        agent.ok("delete_cell", json!({ "notebook_id": notebook, "cell_id": def }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": reader }));
        assert_eq!(read["stale"], true, "the reader is stale straight after the delete: {read}");
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": slow, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": reader }));
        assert_eq!(read["stale"], true, "and stays stale once marimo has done it: {read}");
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": reader, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": reader }));
        assert!(read["errored"] == true && read.to_string().contains("gone_name"), "{read}");
        agent.ok("delete_cell", json!({ "notebook_id": notebook, "cell_id": reader }));
        agent.ok("delete_cell", json!({ "notebook_id": notebook, "cell_id": slow }));
    });

    step("a preview that leaves no cell where it was still keeps its ids", || {
        std::fs::write(folder.join("shift.py"), GROWTH).unwrap();
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let id = agent.ok("open_notebook", json!({ "path": "shift.py" }))["notebook_id"].clone();
        let before = agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].clone();
        agent.ok("delete_cell", json!({ "notebook_id": id, "cell_id": before[0] }));
        agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": before[1] }));
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": before[1], "code": "y = 7\ny" }));
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": false }));
        assert_eq!(agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"], json!([before[1]]));
        agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": before[1], "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": before[1] }));
        assert!(read["errored"] == false && read["output"].as_str().unwrap_or_default().contains('7'), "{read}");
        agent.ok("close_notebook", json!({ "notebook_id": id }));
    });

    step("cells keep their ids when the file changes outside, and after a restart", || {
        let file = folder.join("ids.py");
        std::fs::write(&file, GROWTH).unwrap();
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let id = agent.ok("open_notebook", json!({ "path": "ids.py" }))["notebook_id"].clone();
        let order = |agent: &mut Agent| -> Vec<String> {
            agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_owned()).collect()
        };
        let code = |agent: &mut Agent, cell: &str| agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell, "include_code": true }))["code"].clone();
        let before = order(&mut agent);
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&file, GROWTH.replacen("@app.cell\ndef _():\n    x = 20", "@app.cell\ndef _():\n    w = 0\n    return\n\n\n@app.cell\ndef _():\n    x = 20", 1)).unwrap();
        let now = order(&mut agent);
        assert_eq!(now.len(), 3, "{now:?}");
        assert_eq!(now[1..], before[..], "the cells keep their ids: {now:?} {before:?}");
        assert_eq!(code(&mut agent, &before[0]), "x = 20");
        assert_eq!(code(&mut agent, &now[0]), "w = 0");
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": false }));
        assert_eq!(order(&mut agent), now, "the ids survive going live");
        assert_eq!(code(&mut agent, &before[0]), "x = 20");
        agent.ok("run_all_cells", json!({ "notebook_id": id, "wait_for_completion": true }));
        for cell in &now {
            let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell }));
            assert!(read["errored"] == false && read["not_run"] != true, "{read}");
        }
        assert_eq!(agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": before[1] }))["output"], "21");
        agent.ok("close_notebook", json!({ "notebook_id": id }));

        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let made = agent.ok("new_notebook", json!({ "path": "restarted.py" }));
        let id = made["notebook_id"].clone();
        let file = folder.join("restarted.py");
        let order = |agent: &mut Agent| -> Vec<String> {
            agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_owned()).collect()
        };
        let code = |agent: &mut Agent, cell: &str| agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell, "include_code": true }))["code"].clone();
        let first = made["cell_ids"][0].as_str().unwrap().to_owned();
        agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": first }));
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": first, "code": "x = 20" }));
        let added = agent.ok("add_cell", json!({ "notebook_id": id, "code": "y = x + 1\ny", "after_cell_id": first }))["cell_id"].as_str().unwrap().to_owned();
        let last = agent.ok("add_cell", json!({ "notebook_id": id, "code": "v = y * 2\nv", "after_cell_id": added }))["cell_id"].as_str().unwrap().to_owned();
        agent.ok("run_all_cells", json!({ "notebook_id": id, "wait_for_completion": true }));
        let live = order(&mut agent);
        assert_eq!(live, [first.clone(), added.clone(), last.clone()]);
        agent.ok("restart_notebook", json!({ "notebook_id": id }));
        assert_eq!(order(&mut agent), live, "the ids survive a restart");
        assert_eq!(code(&mut agent, &added), "y = x + 1\ny");
        assert_eq!(code(&mut agent, &last), "v = y * 2\nv");
        agent.ok("run_all_cells", json!({ "notebook_id": id, "wait_for_completion": true }));
        for cell in &live {
            let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell }));
            assert!(read["errored"] == false && read["not_run"] != true, "{read}");
        }
        assert_eq!(agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": last }))["output"], "42");
        let saved = std::fs::read_to_string(&file).unwrap();
        let top = "@app.cell\ndef _():\n    x = 20";
        assert!(saved.contains(top), "{saved}");
        std::fs::write(&file, saved.replacen(top, &format!("@app.cell\ndef _():\n    u = 0\n    return\n\n\n{top}"), 1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut now = order(&mut agent);
        while now.len() == live.len() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            now = order(&mut agent);
        }
        assert_eq!(now[1..], live[..], "the cells keep their ids under marimo too: {now:?} {live:?}");
        assert_eq!(code(&mut agent, &now[0]), "u = 0");
        assert_eq!(code(&mut agent, &last), "v = y * 2\nv");
        agent.ok("close_notebook", json!({ "notebook_id": id }));
    });

    step("a header without marimo gets the pinned marimo, not a loose one", || {
        let file = folder.join("loose.py");
        std::fs::write(&file, GROWTH.replace("#     \"marimo==0.25.1\",\n", "")).unwrap();
        assert!(!std::fs::read_to_string(&file).unwrap().contains("marimo=="));
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let id = agent.ok("open_notebook", json!({ "path": "loose.py" }))["notebook_id"].clone();
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": true }));
        let cells = agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].clone();
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cells[1] }));
        assert!(read["output"].as_str().unwrap_or_default().contains("21"), "{read}");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"marimo==0.25.1\"") && !text.contains("marimo>="), "{text}");
        agent.ok("close_notebook", json!({ "notebook_id": id }));
        let file = folder.join("bare.py");
        let bare = GROWTH.split_once("# ///\n\n").unwrap().1;
        assert!(!bare.contains("# /// script"));
        std::fs::write(&file, bare).unwrap();
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let id = agent.ok("open_notebook", json!({ "path": "bare.py" }))["notebook_id"].clone();
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": true }));
        let cells = agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].clone();
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cells[1] }));
        assert!(read["output"].as_str().unwrap_or_default().contains("21"), "{read}");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# /// script") && text.contains("\"marimo==0.25.1\"") && !text.contains("marimo>="), "{text}");
        agent.ok("close_notebook", json!({ "notebook_id": id }));
    });

    step("a Python notebook's package date moves, and its exact versions with it", || {
        let file = folder.join("dated.py");
        let header = "# /// script\n# requires-python = \">=3.10\"\n# dependencies = [\n#     \"marimo==0.25.1\",\n#     # old on purpose\n#     \"six==1.16.0\",\n# ]\n#\n# [tool.uv]\n# exclude-newer = \"2026-10-05T00:00:00Z\"\n# ///\n";
        let body = GROWTH.split_once("# ///\n\n").unwrap().1.replacen("    y = x + 1\n    y\n", "    import six\n    y = six.__version__\n    y\n", 1);
        std::fs::write(&file, format!("{header}\n{body}")).unwrap();
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let opened = agent.ok("open_notebook", json!({ "path": "dated.py" }));
        let id = opened["notebook_id"].clone();
        let preview = agent.ok("preview_package_update", json!({ "notebook_id": id, "date": "2026-11-01" }));
        assert_eq!(preview["status"], "ready", "{preview}");
        let six = preview["changes"].as_array().unwrap().iter().find(|c| c["name"] == "six").cloned().unwrap_or_else(|| panic!("{preview}"));
        assert!(six["from"] == "1.16.0" && six["change"] == "upgraded", "{preview}");
        let newer = six["to"].as_str().unwrap().to_owned();
        assert!(std::fs::read_to_string(&file).unwrap().contains("\"six==1.16.0\""), "the preview changed nothing");
        let (failed, refused) = agent.call("apply_package_update", json!({ "notebook_id": id, "date": "2026-11-02" }));
        assert!(failed && refused.to_string().contains("no_preview"), "{refused}");
        let applied = agent.ok("apply_package_update", json!({ "notebook_id": id, "date": "2026-11-01" }));
        assert_eq!(applied["process_status"], "waiting_for_permission", "{applied}");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(&format!("\"six=={newer}\"")) && text.contains("exclude-newer = \"2026-11-01T23:59:59Z\"") && text.contains("\"marimo==0.25.1\""), "{text}");
        assert!(text.ends_with(&body), "the cells are as they were: {text}");
        assert!(text.contains("#     # old on purpose\n"), "the header's comment stays: {text}");
        assert!(text.contains("# ]\n#\n# [tool.uv]\n") && !text.contains("# \n"), "a blank line in the header stays `#`: {text}");
        let early = agent.ok("preview_package_update", json!({ "notebook_id": id, "date": "2026-01-01" }));
        assert!(early["status"] == "failed" && early.to_string().contains("marimo 0.25.1"), "{early}");
        let (failed, refused) = agent.call("preview_package_update", json!({ "notebook_id": id, "package": "six" }));
        assert!(failed && refused.to_string().contains("not_supported"), "{refused}");
        let good = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, good.replacen(&format!("\"six=={newer}\""), "\"six==0.0.1\"", 1)).unwrap();
        let broken = agent.ok("preview_package_update", json!({ "notebook_id": id, "date": "2026-11-01" }));
        assert!(broken["status"] == "failed" && broken.to_string().contains("doesn't resolve as it is") && broken["changes"] == json!([]), "{broken}");
        let (failed, refused) = agent.call("apply_package_update", json!({ "notebook_id": id, "date": "2026-11-01" }));
        assert!(failed && refused.to_string().contains("no_preview"), "{refused}");
        std::fs::write(&file, good).unwrap();
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": true }));
        let cells = agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].clone();
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cells[1] }));
        assert!(read["output"].as_str().unwrap_or_default().contains(&newer), "{read}");
        let today = agent.ok("preview_package_update", json!({ "notebook_id": id }));
        assert_eq!(today["status"], "ready", "{today}");
        let day = today["date"].as_str().unwrap().to_owned();
        let applied = agent.ok("apply_package_update", json!({ "notebook_id": id, "date": day }));
        assert!(applied["message"].as_str().unwrap_or_default().contains("restarted"), "{applied}");
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cells[1] }));
        assert_eq!(read["not_run"], true, "{read}");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(&format!("exclude-newer = \"{day}T")), "{text}");
        agent.ok("run_all_cells", json!({ "notebook_id": id, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cells[1] }));
        assert!(read["errored"] == false && read["not_run"] != true, "{read}");
        agent.ok("close_notebook", json!({ "notebook_id": id }));
    });

    step("a new notebook pins marimo and a package date", || {
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let made = agent.ok("new_notebook", json!({ "path": "fresh.py" }));
        assert_eq!(made["execution_allowed"], true, "{made}");
        let text = std::fs::read_to_string(folder.join("fresh.py")).unwrap();
        assert!(text.contains("\"marimo==0.25.1\"") && text.contains("exclude-newer = \""), "{text}");
        let cell = made["cell_ids"][0].as_str().unwrap().to_owned();
        agent.ok("edit_cell", json!({ "notebook_id": made["notebook_id"], "cell_id": cell, "code": "import sys\nsys.version_info[0]" }));
        agent.ok("execute_cell", json!({ "notebook_id": made["notebook_id"], "cell_id": cell, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": made["notebook_id"], "cell_id": cell }));
        assert!(read["output"].as_str().unwrap_or_default().contains('3'), "{read}");
    });

    step("headings keep their own code when the file changes under marimo", || {
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let made = agent.ok("new_notebook", json!({ "path": "headings.py" }));
        let nb = made["notebook_id"].clone();
        let mut last = made["cell_ids"][0].as_str().unwrap().to_owned();
        agent.ok("read_cell", json!({ "notebook_id": nb, "cell_id": last }));
        agent.ok("edit_cell", json!({ "notebook_id": nb, "cell_id": last, "code": "import marimo as mo" }));
        let titles = ["Data Overview", "Data Cleaning", "Growth Curves by Strain", "Cleaned Growth Curves", "Data Quality"];
        let mut headings = Vec::new();
        for (i, title) in titles.iter().enumerate() {
            let added = agent.ok("add_cell", json!({ "notebook_id": nb, "after_cell_id": last, "code": format!("mo.md(\"## {title}\")"), "folded": true }));
            let heading = added["cell_id"].as_str().unwrap().to_owned();
            assert_eq!(added["code"], format!("mo.md(\"\"\"\n## {title}\n\"\"\")"), "the reply gives the code as kept: {added}");
            last = agent.ok("add_cell", json!({ "notebook_id": nb, "after_cell_id": heading, "code": format!("v{i} = {i}\nv{i}") }))["cell_id"].as_str().unwrap().to_owned();
            headings.push(heading);
        }
        agent.ok("submit_changes", json!({ "notebook_id": nb, "wait_for_completion": true }));
        let edited = agent.ok("edit_cell", json!({ "notebook_id": nb, "cell_id": headings[4], "code": "mo.md(\"## Data Quality Checks\")" }));
        assert_eq!(edited["code"], "mo.md(\"\"\"\n## Data Quality Checks\n\"\"\")", "{edited}");
        agent.ok("read_cell", json!({ "notebook_id": nb, "cell_id": headings[3] }));
        let both = agent.ok("edit_cells", json!({ "notebook_id": nb, "cells": [{ "cell_id": headings[3], "code": "mo.md(\"## Cleaned Growth Curves\")" }] }));
        assert_eq!(both["code"][&headings[3]], "mo.md(\"\"\"\n## Cleaned Growth Curves\n\"\"\")", "{both}");
        let latex = r#"mo.md("$\\alpha + \\beta$")"#;
        let raw = "mo.md(r\"\"\"\n$\\alpha + \\beta$\n\"\"\")";
        let added = agent.ok("add_cell", json!({ "notebook_id": nb, "after_cell_id": last, "code": latex }));
        assert_eq!(added["code"], raw, "kept as a raw string: {added}");
        let math = added["cell_id"].as_str().unwrap().to_owned();
        let ends = r#"mo.md("a path: C:\\")"#;
        let added = agent.ok("add_cell", json!({ "notebook_id": nb, "after_cell_id": math, "code": ends }));
        assert!(added.get("code").is_none(), "kept as sent: {added}");
        let path_cell = added["cell_id"].as_str().unwrap().to_owned();
        agent.ok("submit_changes", json!({ "notebook_id": nb, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": nb, "cell_id": path_cell }));
        assert!(read["notes"].as_array().is_some_and(|n| n.iter().any(|n| n.as_str().unwrap_or_default().starts_with("markdown_in_file::"))), "{read}");
        let read = agent.ok("read_cell", json!({ "notebook_id": nb, "cell_id": math, "include_code": true }));
        assert_eq!(read["code"], raw, "{read}");
        assert!(read.get("notes").is_none(), "{read}");
        assert!(!read.to_string().contains('\u{7}'), "no bell in the output: {read}");
        let file = folder.join("headings.py");
        let in_file = "    mo.md(r\"\"\"\n    $\\alpha + \\beta$\n    \"\"\")";
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut text = std::fs::read_to_string(&file).unwrap();
        while !text.contains(in_file) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            text = std::fs::read_to_string(&file).unwrap();
        }
        assert!(text.contains(in_file), "the file keeps the backslashes: {text}");
        std::fs::write(&file, text.replacen("# ///\n", "# ///\n# touched by the test\n", 1)).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let mut titles = titles.to_vec();
        titles[4] = "Data Quality Checks";
        for (heading, title) in headings.iter().zip(titles) {
            let read = agent.ok("read_cell", json!({ "notebook_id": nb, "cell_id": heading, "include_code": true }));
            assert!(read["code"].as_str().unwrap_or_default().contains(&format!("## {title}\n")), "{title}: {read}");
            assert_eq!(read["output_text"].as_str().or(read["output"].as_str()).unwrap_or_default().trim(), title, "{read}");
        }
        agent.ok("close_notebook", json!({ "notebook_id": nb }));
    });

    step("a table's text shows its first 25 rows, so a small table is whole", || {
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let made = agent.ok("new_notebook", json!({ "path": "tables.py" }));
        let nb = made["notebook_id"].clone();
        let first = made["cell_ids"][0].as_str().unwrap().to_owned();
        agent.ok("read_cell", json!({ "notebook_id": nb, "cell_id": first }));
        agent.ok("edit_cell", json!({ "notebook_id": nb, "cell_id": first, "code": "import marimo as mo" }));
        let small = agent.ok("add_cell", json!({ "notebook_id": nb, "after_cell_id": first, "code": "mo.ui.table([{\"t\": i} for i in range(11)])" }))["cell_id"].as_str().unwrap().to_owned();
        let big = agent.ok("add_cell", json!({ "notebook_id": nb, "after_cell_id": small, "code": "mo.ui.table([{\"t\": i} for i in range(30)])" }))["cell_id"].as_str().unwrap().to_owned();
        agent.ok("submit_changes", json!({ "notebook_id": nb, "wait_for_completion": true }));
        let text = |agent: &mut Agent, cell: &str| {
            let read = agent.ok("read_cell", json!({ "notebook_id": nb, "cell_id": cell }));
            read["output_text"].as_str().or(read["output"].as_str()).unwrap_or_default().to_owned()
        };
        let shown = text(&mut agent, &small);
        assert!(shown.lines().any(|l| l.split_whitespace().last() == Some("10")) && !shown.contains("the first"), "all 11 rows: {shown}");
        let shown = text(&mut agent, &big);
        assert!(shown.contains("[the first 25 of 30 rows]") && shown.lines().any(|l| l.split_whitespace().last() == Some("24")) && !shown.lines().any(|l| l.split_whitespace().last() == Some("25")), "{shown}");
        agent.ok("close_notebook", json!({ "notebook_id": nb }));
    });

    step("headings from a hand-written file keep their own code when the file changes under marimo", || {
        let titles = ["Setup", "Data Loading", "Data Cleaning", "Growth Curves by Strain", "Data Quality Checks", "Summary"];
        let mut text = String::from("# /// script\n# requires-python = \">=3.10\"\n# dependencies = [\n#     \"marimo==0.25.1\",\n# ]\n# ///\n\nimport marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App()\n\n\n@app.cell\ndef _():\n    import marimo as mo\n    return (mo,)\n\n");
        for (i, title) in titles.iter().enumerate() {
            text += &format!("\n@app.cell\ndef _(mo):\n    mo.md(\"## {title}\")\n    return\n\n\n@app.cell\ndef _():\n    v{i} = {i}\n    v{i}\n    return\n\n");
        }
        text += "\nif __name__ == \"__main__\":\n    app.run()\n";
        let file = folder.join("handwritten.py");
        std::fs::write(&file, text).unwrap();
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let id = agent.ok("open_notebook", json!({ "path": "handwritten.py" }))["notebook_id"].clone();
        let order: Vec<String> = agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_owned()).collect();
        let headings: Vec<String> = (0..titles.len()).map(|i| order[1 + 2 * i].clone()).collect();
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": headings[5], "include_code": true }));
        assert_eq!(read["code"], "mo.md(\"\"\"\n## Summary\n\"\"\")", "{read}");
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": false }));
        agent.ok("run_all_cells", json!({ "notebook_id": id, "wait_for_completion": true }));
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": headings[5], "code": "mo.md(\"## Summary of results\")" }));
        agent.ok("submit_changes", json!({ "notebook_id": id, "wait_for_completion": true }));
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.contains("mo.md(\"\"\"\n    ## Data Cleaning\n    \"\"\")"), "marimo saved the file in its form: {saved}");
        std::fs::write(&file, saved.replacen("# ///\n", "# ///\n# touched by the test\n", 1)).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let mut titles = titles.to_vec();
        titles[5] = "Summary of results";
        for (heading, title) in headings.iter().zip(titles) {
            let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": heading, "include_code": true }));
            assert!(read["code"].as_str().unwrap_or_default().contains(&format!("## {title}\n")), "{title}: {read}");
            assert_eq!(read["output_text"].as_str().or(read["output"].as_str()).unwrap_or_default().trim(), title, "{read}");
        }
        agent.ok("close_notebook", json!({ "notebook_id": id }));
    });

    step("a typo's import installs nothing without the user's OK", || {
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let made = agent.ok("new_notebook", json!({ "path": "typo.py" }));
        let id = made["notebook_id"].clone();
        let cell = made["cell_ids"][0].as_str().unwrap().to_owned();
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "import pandsa" }));
        let ran = agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": cell, "wait_for_completion": true }));
        assert_eq!(ran["packages"]["step"], "asking", "{ran}");
        assert_eq!(ran["packages"]["download"]["packages"], json!([{ "package": "pandsa", "close_to": "pandas" }]), "{ran}");
        assert!(ran["message"].as_str().unwrap_or_default().contains("pandsa is close to pandas, a widely used package"), "{ran}");
        assert!(!header(&folder.join("typo.py")).contains("pandsa"), "nothing installed");
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "import json\njson.dumps(1)" }));
        let began = Instant::now();
        let ran = agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": cell, "wait_for_completion": true }));
        assert!(began.elapsed() < Duration::from_secs(15), "{:?}", began.elapsed());
        assert!(ran.get("packages").is_none_or(Value::is_null), "{ran}");
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell }));
        assert!(read["errored"] == false && read["output"].as_str().unwrap_or_default().contains('1'), "{read}");
        let refused = agent.call("approve_install", json!({ "notebook_id": id }));
        assert!(refused.0 && refused.1.to_string().contains("nothing_to_approve"), "{refused:?}");
        agent.ok("close_notebook", json!({ "notebook_id": id }));
    });

    step("an install keeps to the notebook's package date", || {
        const DATE: &str = "2026-10-05T00:00:00Z";
        let name = format!("endeavor-e2e-dated-{}.py", std::process::id());
        for old in std::fs::read_dir(work.join("cache/uv/environments-v2")).into_iter().flatten().flatten() {
            if old.file_name().to_string_lossy().starts_with("endeavor-e2e-dated-") {
                let _ = std::fs::remove_dir_all(old.path());
            }
        }
        std::fs::write(folder.join(&name), GROWTH.replace("#     \"marimo==0.25.1\",\n# ]\n", &format!("#     \"marimo==0.25.1\",\n# ]\n#\n# [tool.uv]\n# exclude-newer = \"{DATE}\"\n"))).unwrap();
        let expected = Command::new("uv").args(["pip", "compile", "--quiet", "--no-header", "--no-annotate", "--exclude-newer", DATE, "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().and_then(|mut uv| {
            uv.stdin.take().unwrap().write_all(b"boto3\n")?;
            uv.wait_with_output()
        });
        let expected = String::from_utf8(expected.unwrap().stdout).unwrap().lines().find(|l| l.starts_with("boto3==")).unwrap().to_owned();
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let opened = agent.ok("open_notebook", json!({ "path": name }));
        let id = opened["notebook_id"].clone();
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": false }));
        let cell = agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"][1].as_str().unwrap().to_owned();
        agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell }));
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "import boto3" }));
        let user = agent.ok("add_cell", json!({ "notebook_id": id, "code": "boto3.__version__", "after_cell_id": cell }))["cell_id"].as_str().unwrap().to_owned();
        agent.ok("submit_changes", json!({ "notebook_id": id, "wait_for_completion": false }));
        let packages = |agent: &mut Agent| {
            let listed = agent.ok("list_notebooks", json!({}));
            listed.as_array().unwrap().iter().find(|n| n["notebook_id"] == id).unwrap()["packages"].clone()
        };
        let until = |agent: &mut Agent, step: &str| {
            let began = Instant::now();
            loop {
                let now = packages(agent);
                if now["step"] == step || began.elapsed() > Duration::from_secs(30) {
                    break now;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        let asking = until(&mut agent, "asking");
        assert!(asking["new"] == true && asking["download"]["packages"] == json!([{ "package": "boto3" }]), "it asks about boto3: {asking}");
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": user }));
        assert!(read["errored"] == false && read["queued"] == true, "a cell waiting for an OK doesn't show the import error: {read}");
        assert!(read["message"].as_str().unwrap_or_default().contains("Python has to install packages this notebook doesn't have yet: boto3"), "{read}");
        assert!(!header(&folder.join(&name)).contains("boto3"), "nothing installed before the OK");
        let stale = agent.ok("approve_install", json!({ "notebook_id": id, "packages": ["pandas"] }));
        assert!(stale["approved"] == false && stale["accepted"] == false && stale["asking"] == json!(["boto3"]) && stale["process_status"].is_string(), "{stale}");
        assert!(stale["message"].as_str().unwrap_or_default().contains("Nothing was approved"), "{stale}");
        assert!(until(&mut agent, "asking")["download"]["packages"] == json!([{ "package": "boto3" }]), "it still asks");
        let extra = agent.ok("add_cell", json!({ "notebook_id": id, "code": "import endeavor_e2e_absent", "after_cell_id": user }))["cell_id"].as_str().unwrap().to_owned();
        agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": extra, "wait_for_completion": false }));
        let began = Instant::now();
        while until(&mut agent, "asking")["download"]["packages"].as_array().map_or(0, Vec::len) < 2 && began.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let approved = agent.ok("approve_install", json!({ "notebook_id": id, "packages": ["Boto3"] }));
        assert!(approved["approved"] == true && approved["packages"] == json!(["boto3"]) && approved["asking"] == json!(["endeavor_e2e_absent"]), "{approved}");
        assert!(approved["accepted"] == false && approved["process_status"].is_string(), "{approved}");
        assert!(approved["message"].as_str().unwrap_or_default().contains("needs their OK of its own"), "{approved}");
        assert!(until(&mut agent, "asking")["download"]["packages"] == json!([{ "package": "endeavor_e2e_absent" }]), "the other still asks");
        assert!(!header(&folder.join(&name)).contains("boto3"), "nothing installed while one waits");
        agent.ok("delete_cell", json!({ "notebook_id": id, "cell_id": extra }));
        agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": cell, "wait_for_completion": false }));
        let installing = until(&mut agent, "installing");
        assert!(installing["packages"].as_array().is_some_and(|p| p.iter().any(|p| p == "boto3")), "the install says it installs boto3: {installing}");
        let mut read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": user }));
        for _ in 0..20 {
            assert!(read["errored"] == false, "a cell waiting for its package doesn't show the import error: {read}");
            if read["queued"] != true && read["running"] != true {
                break;
            }
            read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": user }));
        }
        let importer = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell }));
        assert!(importer["errored"] == false, "{importer}");
        let version = expected.trim_start_matches("boto3==");
        assert!(read["output"].as_str().unwrap_or_default().contains(version), "boto3 {version}, as of {DATE}: {read}");
        let header = std::fs::read_to_string(folder.join(&name)).unwrap();
        assert!(header.contains(&format!("\"{expected}\"")), "the header names {expected}: {header}");
        agent.ok("close_notebook", json!({ "notebook_id": id }));
        let opened = agent.ok("open_notebook", json!({ "path": name }));
        let id = opened["notebook_id"].clone();
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": false }));
        let user = agent.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"][2].as_str().unwrap().to_owned();
        agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": user }));
        agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": user, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": user }));
        assert!(read["errored"] == false && read["output"].as_str().unwrap_or_default().contains(version), "{read}");
    });

    step("the browser reaches marimo's page, and not its terminal or file listings", || {
        let target = format!("/marimo/?file={}", path.replace(' ', "%20"));
        let (status, _, _) = get(port, &target, "");
        assert_eq!(status, "HTTP/1.1 401 Unauthorized", "no key, no page");
        let (status, head, _) = get(port, &format!("{target}&token={pages}"), "");
        assert_eq!(status, "HTTP/1.1 303 See Other", "{head}");
        let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}"));
        let cookie = format!("Cookie: {}\r\nSec-Fetch-Site: none\r\n", set.split(';').next().unwrap());
        let (status, head, page) = get(port, &target, &cookie);
        assert_eq!(status, "HTTP/1.1 200 OK", "{head}\n{page}");
        assert!(page.contains("marimo") && page.contains("serverToken"), "marimo's page: {}", &page[..page.len().min(400)]);
        assert!(!head.to_ascii_lowercase().contains("set-cookie"), "marimo's cookies stay behind the port: {head}");
        for refused in ["/marimo/terminal/ws", "/marimo/api/home/workspace_files", "/marimo/api/files/list_files"] {
            let (status, _, _) = get(port, refused, &cookie);
            assert_eq!(status, "HTTP/1.1 404 Not Found", "{refused}");
        }
        let (status, head, _) = get(port, "/marimo", &cookie);
        assert!(status.starts_with("HTTP/1.1 301") && head.contains("Location: /marimo/"), "{head}");
    });

    step("an agent's edit reaches the page that edits the notebook", || {
        let (_, head, _) = get(port, &format!("/marimo/?file={}&token={pages}", path.replace(' ', "%20")), "");
        let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap();
        let cookie = set.split(';').next().unwrap().to_owned();
        let url = format!("ws://localhost:{port}/marimo/ws?session_id=page-e2e&file={}", path.replace(' ', "%20"));
        let mut page = Command::new("uv")
            .args(["run", "--quiet", "--no-project", "--with", "websockets==15.0.1", "python", "-c", PAGE, &url, &cookie, "x = 5"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let said = lines(page.stdout.take().unwrap());
        assert_eq!(said.recv_timeout(Duration::from_secs(60)).as_deref(), Ok("main"), "the page holds the session");
        agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0] }));
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 5" }));
        let got = said.recv_timeout(Duration::from_secs(20));
        let _ = page.kill();
        let _ = page.wait();
        assert_eq!(got.as_deref(), Ok("got"), "the page saw the agent's edit");
    });

    step("a cell disabled on the page stays disabled after an agent's edit", || {
        let file = path.replace(' ', "%20");
        let (_, head, _) = get(port, &format!("/marimo/?file={file}&token={pages}"), "");
        let cookie = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap().split(';').next().unwrap().to_owned();
        let (_, _, html) = get(port, &format!("/marimo/?file={file}"), &format!("Cookie: {cookie}\r\nSec-Fetch-Site: none\r\n"));
        let token = html.split("serverToken").nth(1).and_then(|rest| rest.split('"').find(|part| part.len() > 8 && !part.contains([':', ',', ' ']))).unwrap_or_else(|| panic!("no server token in the page")).to_owned();
        let page = Command::new("uv")
            .args(["run", "--quiet", "--no-project", "--with", "websockets==15.0.1", "python", "-c", PAGE_DISABLES, &format!("localhost:{port}"), "page-disables", &cookie, &token, &order[1], &file])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&page.stdout).trim(), "200", "marimo took the change: {}", String::from_utf8_lossy(&page.stderr));
        agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0] }));
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 6" }));
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut saved = std::fs::read_to_string(&path).unwrap();
        while !saved.contains("x = 6") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            saved = std::fs::read_to_string(&path).unwrap();
        }
        assert!(saved.contains("x = 6") && saved.contains("@app.cell(disabled=True)\ndef _(x):"), "the agent's edit is saved and the cell stays disabled: {saved}");
        assert!(saved.contains("@app.cell(disabled=True)\ndef _():\n    z = 1"), "a cell made disabled on the page stays disabled: {saved}");
    });

    step("stopping the runtime stops marimo", || {
        let marimo: Value = serde_json::from_str(&std::fs::read_to_string(state.join("marimo.json")).unwrap()).unwrap();
        let page = marimo["marimo_port"].as_u64().unwrap() as u16;
        let mut child = cleanup.children.pop().unwrap();
        // SAFETY: plain syscall to our own child.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(("127.0.0.1", page)).is_ok() {
            assert!(Instant::now() < deadline, "marimo still answers on {page}");
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(!state.join("marimo.json").exists());
    });
}

#[test]
#[ignore = "starts real marimo: cargo test -p endeavor-mcp --test e2e_marimo -- --ignored"]
fn a_plugin_session_opens_a_python_notebook_on_the_apps_runtime() {
    use common::helper::Helper;
    if !has_uv() {
        eprintln!("SKIPPED: no uv.");
        return;
    }
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-marimo-app");
    if let Some(pid) = recorded_pid(&work.join("state")) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    let state = fresh(work.join("state"));
    let folder = fresh(work.join("project"));
    let _cleanup = Cleanup { state: state.clone(), children: Vec::new() };
    std::fs::write(folder.join("growth.py"), GROWTH).unwrap();
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime").canonicalize().unwrap();
    let connect = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
        command
            .args(["connect", "--state-dir"])
            .arg(&state)
            .args(["--julia", work.join("no julia here").to_str().unwrap(), "--julia-when-needed", "--runtime"])
            .arg(&runtime)
            .args(["--depot", &format!("{}:", work.join("depot").display()), "--own-r", "--any-node", "--quit-with-client"])
            .env("XDG_CACHE_HOME", work.join("cache"))
            .env("XDG_STATE_HOME", work.join("state-home"))
            .env("XDG_CONFIG_HOME", work.join("config"));
        command
    };
    let mut helper = Helper::spawn(connect());
    helper.hello();
    helper.request_start(None, false);
    let token = step("the app's runtime starts without Julia", || loop {
        match helper.next_within(Duration::from_secs(120)) {
            wire::ToApp::Ready { token, .. } => break token,
            wire::ToApp::Found { .. } | wire::ToApp::Progress { .. } => {}
            other => panic!("expected Ready, got {other:?}"),
        }
    });
    let call = |client: &str, name: &str, arguments: Value| {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string();
        let mut socket = helper.connect();
        socket.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
        write!(
            socket,
            "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Endeavor-Session: {client}-1\r\nX-Endeavor-Client: {client}\r\n{}Content-Length: {}\r\n\r\n{body}",
            if client == "plugin" { format!("X-Endeavor-Front-Pid: {}\r\n", std::process::id()) } else { String::new() },
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(reply.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap_or_else(|_| panic!("{reply}"));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        (reply["result"]["isError"] == true, text)
    };
    let path = folder.join("growth.py");
    let path = path.to_str().unwrap();
    step("the app's session is refused the Python notebook", || {
        let (failed, text) = call("app", "open_notebook", json!({ "path": path }));
        assert!(failed && text.contains("This version of the Endeavor app"), "{text}");
    });
    let notebook = step("the plugin's session opens it there", || {
        let (failed, text) = call("plugin", "open_notebook", json!({ "path": path }));
        assert!(!failed, "{text}");
        let opened: Value = serde_json::from_str(&text).unwrap();
        assert!(opened["notebook_id"].is_string(), "{opened}");
        let (failed, text) = call("plugin", "get_cell_order", json!({ "notebook_id": opened["notebook_id"] }));
        assert!(!failed && text.contains("cell_ids"), "marimo answers through the app's core: {text}");
        opened["notebook_id"].as_str().unwrap().to_owned()
    });
    step("the app quits with Keep running off, and the runtime stays for the plugin until it is idle", || {
        let pid = recorded_pid(&state).unwrap();
        helper.send(wire::ToHelper::Stop { id: 9, force: false, unless_shared: true });
        helper.exits();
        // SAFETY: signal 0 only checks that the process exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the runtime still runs");
        let record: Value = serde_json::from_str(&std::fs::read_to_string(state.join("runtime.json")).unwrap()).unwrap();
        assert_eq!((record["pid"].as_i64(), &record["exits_when_idle"]), (Some(i64::from(pid)), &json!(true)), "{record}");
        let port = record["port"].as_u64().unwrap() as u16;
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "get_cell_order", "arguments": { "notebook_id": notebook } } }).to_string();
        let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(socket, "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Endeavor-Session: plugin-1\r\nX-Endeavor-Client: plugin\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        assert!(reply.contains("cell_ids"), "the plugin's notebook is still open: {reply}");
    });
    step("the app opens again and takes the runtime back, so no front takes it for the plugin's own", || {
        let pid = recorded_pid(&state).unwrap();
        let again = Helper::spawn(connect());
        again.hello();
        again.request_start(None, false);
        loop {
            match again.next_within(Duration::from_secs(120)) {
                wire::ToApp::Ready { reattached, pid: now, .. } => {
                    assert!(reattached && now == pid as u32, "the same runtime: {now}");
                    break;
                }
                wire::ToApp::Found { .. } | wire::ToApp::Progress { .. } => {}
                other => panic!("expected Ready, got {other:?}"),
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let record: Value = serde_json::from_str(&std::fs::read_to_string(state.join("runtime.json")).unwrap()).unwrap();
            if record["exits_when_idle"] == json!(false) {
                break;
            }
            assert!(Instant::now() < deadline, "{record}");
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}
