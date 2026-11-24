#![cfg(windows)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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

fn fresh(path: PathBuf) -> PathBuf {
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn has_uv() -> bool {
    Command::new("uv").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn running(pid: u64) -> bool {
    let listed = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output().unwrap();
    String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""))
}

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2)).is_ok()
}

struct Cleanup {
    state: PathBuf,
    serve: Option<Child>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let log = self.state.join("runtime.log");
            eprintln!("── the runtime's log, {}:\n{}", log.display(), std::fs::read_to_string(&log).unwrap_or_else(|e| format!("(not read: {e})")));
        }
        let _ = Command::new(env!("CARGO_BIN_EXE_endeavor")).args(["stop", "--force", "--state-dir"]).arg(&self.state).output();
        if let Some(mut serve) = self.serve.take() {
            let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &serve.id().to_string()]).output();
            let _ = serve.wait();
        }
    }
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
        let body = json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params }).to_string();
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(600))).unwrap();
        let session = self.session.as_ref().map_or(String::new(), |id| format!("Mcp-Session-Id: {id}\r\n"));
        write!(
            socket,
            "POST /mcp HTTP/1.0\r\nHost: localhost:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nX-Endeavor-Consent: front\r\n{session}Content-Length: {}\r\n\r\n{body}",
            self.port,
            self.token,
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        assert_eq!(head.lines().next(), Some("HTTP/1.1 200 OK"), "{method}: {body}");
        if method == "initialize" {
            self.session = head.lines().find_map(|line| line.strip_prefix("Mcp-Session-Id: ")).map(str::to_owned);
        }
        serde_json::from_str(body).unwrap()
    }

    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.mcp("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {reply}"));
        assert!(reply["result"]["isError"] != true, "{name}({arguments}): {text}");
        serde_json::from_str(text).unwrap_or_else(|_| json!(text))
    }

    fn started(&mut self, name: &str, arguments: Value) -> Value {
        let began = Instant::now();
        loop {
            let result = self.ok(name, arguments.clone());
            if result["error"] != "python_installing" {
                return result;
            }
            assert!(began.elapsed() < Duration::from_secs(600), "marimo didn't start: {result}");
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

fn serve(state: &Path, folder: &Path) -> (Child, Vec<String>) {
    let mut serve = Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .args(["serve", "--folder"])
        .arg(folder)
        .arg("--state-dir")
        .arg(state)
        .arg("--julia")
        .arg(state.join("no julia here.exe"))
        .arg("--depot")
        .arg(state.join("depot"))
        .env("ENDEAVOR_TEST_MARIMO_WAIT_SECS", "5")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = BufReader::new(serve.stdout.take().unwrap());
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in out.lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut printed = Vec::new();
    loop {
        match lines.recv_timeout(Duration::from_secs(120)) {
            Ok(line) if line == "Press Ctrl-C to stop Endeavor." => return (serve, printed),
            Ok(line) => printed.push(line),
            Err(_) => panic!("serve wasn't up within two minutes; it printed {printed:?}"),
        }
    }
}

#[test]
#[ignore = "starts real marimo: cargo test -p endeavor-mcp --test e2e_marimo_windows -- --ignored"]
fn a_python_notebook_runs_on_windows_and_stop_ends_marimo() {
    if !has_uv() {
        eprintln!("SKIPPED: no uv.");
        return;
    }
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-marimo-windows");
    let state = fresh(work.join("state"));
    let folder = fresh(work.join("project"));
    std::fs::write(folder.join("growth.py"), GROWTH).unwrap();
    let mut cleanup = Cleanup { state: state.clone(), serve: None };
    let (child, printed) = serve(&state, &folder);
    cleanup.serve = Some(child);
    let link = printed.iter().find_map(|l| l.trim().strip_prefix("http://localhost:")).filter(|l| l.contains("/?token=")).unwrap_or_else(|| panic!("{printed:?}"));
    let port: u16 = link.split_once("/?token=").unwrap().0.parse().unwrap();
    let token = printed.iter().find_map(|l| l.trim().strip_prefix("Header: Authorization: Bearer ")).unwrap().to_owned();
    let mut agent = Agent { port, token, id: 0, session: None };
    agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));

    let opened = agent.started("open_notebook", json!({ "path": "growth.py" }));
    let notebook = opened["notebook_id"].as_str().unwrap_or_else(|| panic!("{opened}")).to_owned();
    let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }))["cell_ids"].clone();
    let order: Vec<String> = order.as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
    assert_eq!(order.len(), 2, "{order:?}");
    agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": order[0], "code": "x = 41" }));
    agent.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
    agent.ok("run_all_cells", json!({ "notebook_id": notebook, "wait_for_completion": true }));
    let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": order[1] }));
    assert!(read["errored"] == false && read["output"].as_str().unwrap_or_default().contains("42"), "{read}");
    assert!(std::fs::read_to_string(folder.join("growth.py")).unwrap().contains("x = 41"), "marimo saved the edit");

    let made = agent.started("new_notebook", json!({ "path": "decay.py" }));
    assert!(made["notebook_id"].is_string(), "{made}");
    assert!(folder.join("decay.py").is_file());

    let marimo: Value = serde_json::from_str(&std::fs::read_to_string(state.join("marimo.json")).unwrap()).unwrap();
    let (adapter, page, bridge) = (marimo["pid"].as_u64().unwrap(), marimo["marimo_port"].as_u64().unwrap() as u16, marimo["bridge_port"].as_u64().unwrap() as u16);
    assert!(running(adapter) && listening(page) && listening(bridge), "{marimo}");
    let stopped = Command::new(env!("CARGO_BIN_EXE_endeavor")).args(["stop", "--state-dir"]).arg(&state).output().unwrap();
    assert!(stopped.status.success(), "{}", String::from_utf8_lossy(&stopped.stderr));
    let by = Instant::now() + Duration::from_secs(30);
    while Instant::now() < by && (running(adapter) || listening(page) || listening(bridge)) {
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(!running(adapter), "the adapter outlived the runtime");
    assert!(!listening(page) && !listening(bridge), "marimo's server outlived the runtime");
}
