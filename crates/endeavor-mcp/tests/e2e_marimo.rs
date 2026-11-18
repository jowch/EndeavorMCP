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
    write!(
        socket,
        "POST /mcp HTTP/1.0\r\nHost: localhost:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\n\r\n{body}",
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
        id
    });

    let order = step("read, then run in order; an edit in safe preview is refused", || {
        let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }))["cell_ids"].clone();
        let order: Vec<String> = order.as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
        assert_eq!(order.len(), 2, "{order:?}");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0] }));
        assert_eq!(read["code"], "x = 20", "{read}");
        // ponytail: marimo has no session in safe preview, so nothing can save an edit (#351).
        let (failed, refused) = agent.call("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 41" }));
        assert!(failed || refused.get("error").is_some(), "{refused}");
        agent.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
        agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[0] }));
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 41" }));
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
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": c, "code": "1 / 0" }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": c, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": c }));
        assert_eq!(read["errored"], true, "{read}");
        assert!(read.to_string().contains("ZeroDivisionError"), "{read}");
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
