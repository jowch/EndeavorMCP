//! A Python notebook end to end, with real marimo and no Julia: `endeavor serve`, then an agent opens a
//! marimo notebook, which starts Python notebooks' adapter (`runtime/marimo/adapter.py`) through uv; it
//! reads, edits and runs cells, makes a new notebook, and the browser reaches marimo's page at
//! `/marimo/` through the runtime's port.
//!
//! Ignored by default, like e2e_r; it needs `uv` (on the PATH or in ~/.local/bin) and PyPI. The first run
//! downloads marimo and the notebooks' packages into uv's cache.
//!
//!     cargo test -p endeavor-mcp --test e2e_marimo -- --ignored --nocapture

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

/// Whatever happens, no runtime stays behind; a failed test prints the runtime's log.
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
    // The agent stands in for a client behind the plugin's front, which asks the user itself: so its
    // approve_install is the user's yes (#392).
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

/// A GET on the runtime's port: the status line, the head and the body.
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

/// A notebook's PEP 723 header, where marimo writes what it installs.
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

/// A stand-in for marimo's page: it takes the session as its editor, says `main`, then `got` once an edit
/// with the given code arrives.
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
        // The install step reads while a package installs, which a warm uv cache would otherwise finish first.
        .env("ENDEAVOR_TEST_MARIMO_INSTALL_DELAY_SECS", "3")
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
        let opened = agent.ok("open_notebook", json!({ "path": "growth.py" }));
        assert_eq!((&opened["path"], &opened["execution_allowed"]), (&json!(path), &json!(false)), "{opened}");
        let id = opened["notebook_id"].as_str().unwrap().to_owned();
        let encoded = path.replace(' ', "%20");
        assert_eq!(opened["browser_url"], json!(format!("http://localhost:{port}/marimo/?file={encoded}")), "{opened}");
        let marimo = std::fs::read_to_string(state.join("marimo.json")).expect("the adapter wrote its state");
        assert!(marimo.contains("marimo_port") && !marimo.contains("token"), "the core made marimo's token: {marimo}");
        // Opening the notebook's page would build its environment, which runs code: not before it may run.
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
        // In safe preview marimo has no session: an edit is written with marimo's own save, and nothing builds (#351).
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 41" }));
        let added = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "w = 1", "after_cell_id": order[0] }))["cell_id"].as_str().unwrap().to_owned();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("x = 41") && saved.contains("w = 1") && saved.contains("#     \"marimo==0.25.1\","), "the preview saved the edits and kept the header: {saved}");
        let preview: Value = agent.ok("get_cell_order", json!({ "notebook_id": notebook }))["cell_ids"].clone();
        assert_eq!(preview.as_array().unwrap().len(), 3, "{preview}");
        agent.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
        // marimo numbers a file's cells by position when it loads it; the ids the preview gave out still hold.
        assert_eq!(agent.ok("get_cell_order", json!({ "notebook_id": notebook }))["cell_ids"], preview, "the ids survive going live");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "include_code": true }));
        assert_eq!(read["code"], "x = 41", "{read}");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": added, "include_code": true }));
        assert_eq!(read["code"], "w = 1", "{read}");
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": added, "code": "w = 2" }));
        assert!(std::fs::read_to_string(&path).unwrap().contains("w = 2"), "the live session knows the added cell by its preview id");
        // The ancestor was edited and never ran: the run takes it first.
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
        // A run takes the cells below it, as in Pluto: marimo's lazy mode alone would leave them stale
        // (smoke P2 with Haiku, 11-19).
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
    });

    step("a preview that leaves no cell where it was still keeps its ids", || {
        // Every cell moves: the first is deleted, so marimo's ids for the file share none with the preview's.
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

    step("a new notebook pins marimo and a package date", || {
        // A session works on one notebook: another agent makes it.
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

    step("a typo's import installs nothing without the user's OK", || {
        // PyPI isn't curated: `import pandsa` would install whatever is published under that name (#355).
        let mut agent = Agent { port, token: agent.token.clone(), id: 0, session: None };
        agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let made = agent.ok("new_notebook", json!({ "path": "typo.py" }));
        let id = made["notebook_id"].clone();
        let cell = made["cell_ids"][0].as_str().unwrap().to_owned();
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "import pandsa" }));
        let ran = agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": cell, "wait_for_completion": true }));
        assert_eq!(ran["packages"]["step"], "asking", "{ran}");
        // pandsa is a letter swap from pandas, which the question and the message say (#416).
        assert_eq!(ran["packages"]["download"]["packages"], json!([{ "package": "pandsa", "close_to": "pandas" }]), "{ran}");
        assert!(ran["message"].as_str().unwrap_or_default().contains("pandsa is close to pandas, a widely used package"), "{ran}");
        assert!(!header(&folder.join("typo.py")).contains("pandsa"), "nothing installed");
        // The agent fixes the typo and runs again: nothing new, nothing asks, and the cell runs.
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "import json\njson.dumps(1)" }));
        // It runs at once: a cell that waits for an OK isn't waited for as a run (#355).
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
        // marimo installs with `uv pip install`, which reads no header; the date has to reach it. boto3 has a
        // release most days, so the newest is never the one of a date weeks back.
        const DATE: &str = "2026-10-05T00:00:00Z";
        // uv keeps a script's environment by its path, so a notebook of an earlier run's name would have
        // boto3 already, and nothing would install: each run's notebook has a name of its own, and the
        // environments of earlier runs' go (marimo's uv uses the runtime's cache, under XDG_CACHE_HOME).
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
        // Run as an agent usually does, without waiting, then read: the read waits through the install and
        // the rerun, and never shows the ModuleNotFoundError the first run hit (smoke N7 with Haiku, 11-19).
        // The cell that uses the import waits too, and runs again after it: it doesn't keep its
        // "an ancestor raised" (smoke P1, 11-19).
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
        // boto3 is new to the notebook: nothing installs until the user's OK (#355), and the cells wait.
        let asking = until(&mut agent, "asking");
        assert!(asking["new"] == true && asking["download"]["packages"] == json!([{ "package": "boto3" }]), "it asks about boto3: {asking}");
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": user }));
        assert!(read["errored"] == false && read["queued"] == true, "a cell waiting for an OK doesn't show the import error: {read}");
        assert!(read["message"].as_str().unwrap_or_default().contains("Python has to install packages this notebook doesn't have yet: boto3"), "{read}");
        assert!(!header(&folder.join(&name)).contains("boto3"), "nothing installed before the OK");
        // A yes to a question that named other packages approves nothing (#418).
        let stale = agent.ok("approve_install", json!({ "notebook_id": id, "packages": ["pandas"] }));
        assert!(stale["approved"] == false && stale["asking"] == json!(["boto3"]), "{stale}");
        assert!(stale["message"].as_str().unwrap_or_default().contains("Nothing was approved"), "{stale}");
        assert!(until(&mut agent, "asking")["download"]["packages"] == json!([{ "package": "boto3" }]), "it still asks");
        // The notebook comes to need another package before the user's yes to boto3: the yes covers boto3
        // only (names compare as PyPI's do), and the other asks on its own.
        let extra = agent.ok("add_cell", json!({ "notebook_id": id, "code": "import endeavor_e2e_absent", "after_cell_id": user }))["cell_id"].as_str().unwrap().to_owned();
        agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": extra, "wait_for_completion": false }));
        let began = Instant::now();
        while until(&mut agent, "asking")["download"]["packages"].as_array().map_or(0, Vec::len) < 2 && began.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let approved = agent.ok("approve_install", json!({ "notebook_id": id, "packages": ["Boto3"] }));
        assert!(approved["approved"] == true && approved["packages"] == json!(["boto3"]) && approved["asking"] == json!(["endeavor_e2e_absent"]), "{approved}");
        assert!(approved["message"].as_str().unwrap_or_default().contains("needs their OK of its own"), "{approved}");
        assert!(until(&mut agent, "asking")["download"]["packages"] == json!([{ "package": "endeavor_e2e_absent" }]), "the other still asks");
        assert!(!header(&folder.join(&name)).contains("boto3"), "nothing installed while one waits");
        // The other is dropped: boto3, which the user said yes to, installs on the next run without asking.
        agent.ok("delete_cell", json!({ "notebook_id": id, "cell_id": extra }));
        agent.ok("execute_cell", json!({ "notebook_id": id, "cell_id": cell, "wait_for_completion": false }));
        // The install is under way (slowed by ENDEAVOR_TEST_MARIMO_INSTALL_DELAY_SECS) and says what it installs.
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
        // The header builds again: a version newer than its date wouldn't resolve.
        agent.ok("close_notebook", json!({ "notebook_id": id }));
        let opened = agent.ok("open_notebook", json!({ "path": name }));
        let id = opened["notebook_id"].clone();
        agent.ok("allow_execution", json!({ "notebook_id": id, "run_notebook": false }));
        // A reload gives marimo's cells new ids (#353); the cell that uses boto3 is still the third.
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
        // marimo sends an edit to every consumer but the one it came from. The page holds the session, so an
        // edit sent in its name never reached it, and its next autosave put back what it had.
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

/// The app's runtime on this computer (`connect --own-r`, as the app starts it) and a plugin session that
/// shares it (#358): the plugin's session opens a Python notebook there, through the core's own marimo,
/// and the app's own sessions are refused one.
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
    let helper = Helper::spawn(command);
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
            "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Endeavor-Session: {client}-1\r\nX-Endeavor-Client: {client}\r\nContent-Length: {}\r\n\r\n{body}",
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
        assert!(failed && text.contains("not yet in the Endeavor app"), "{text}");
    });
    step("the plugin's session opens it there", || {
        let (failed, text) = call("plugin", "open_notebook", json!({ "path": path }));
        assert!(!failed, "{text}");
        let opened: Value = serde_json::from_str(&text).unwrap();
        assert!(opened["notebook_id"].is_string(), "{opened}");
        let (failed, text) = call("plugin", "get_cell_order", json!({ "notebook_id": opened["notebook_id"] }));
        assert!(!failed && text.contains("cell_ids"), "marimo answers through the app's core: {text}");
    });
}
