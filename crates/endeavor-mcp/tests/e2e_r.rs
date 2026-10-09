//! An R notebook end to end, with real R and Julia: `endeavor serve`, then
//! an agent opens an Ember notebook, which starts R's adapter with Ember in it;
//! it reads, edits and runs a cell, and the browser reaches Ember's page at
//! `/ember/` through the runtime's port. R notebooks aren't open to agents yet
//! (their tools and skills come later), so the test lets them in with
//! ENDEAVOR_TEST_R_NOTEBOOKS.
//!
//! Ignored by default, like e2e_julia (which says where Julia comes from); R is
//! `Rscript` on the PATH with Ember installed in its library:
//!
//!     cargo test -p endeavor-mcp --test e2e_r -- --ignored --nocapture

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::find_julia;
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

/// Whatever happens, no Julia stays behind.
struct Cleanup {
    state: PathBuf,
    children: Vec<Child>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(pid) = recorded_pid(&self.state) {
            // SAFETY: plain syscall; the core leads Julia's process group.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
    }
}

/// `endeavor ARGS` with the test's own state, cache, depot and julia.
fn command(args: &[&str], work: &Path, julia: &Path, depot: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command
        .args(args)
        .arg("--state-dir")
        .arg(work.join("state"))
        .env("XDG_CACHE_HOME", work.join("cache"))
        .env("XDG_STATE_HOME", work.join("state-home"))
        .env("XDG_CONFIG_HOME", work.join("config"))
        .env("ENDEAVOR_IDLE_CHECK_SECS", "1");
    if args[0] != "stop" {
        command.args(["--julia", julia.to_str().unwrap(), "--depot", depot]);
    }
    command
}

/// Lines of a child's stream as they come.
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

/// POST a JSON-RPC message to `/mcp` with the bearer token and, once it has
/// one, the session id, as an agent configured from serve's output does: the
/// response's head and body.
fn post(port: u16, token: &str, session: Option<&str>, message: &Value) -> (String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
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
    /// The `Mcp-Session-Id` from `initialize`, sent back on every request after.
    session: Option<String>,
}

impl Agent {
    fn new(port: u16, token: &str) -> Agent {
        Agent { port, token: token.to_owned(), id: 0, session: None }
    }

    fn mcp(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let (head, body) = post(self.port, &self.token, self.session.as_deref(), &json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params }));
        assert_eq!(head.lines().next(), Some("HTTP/1.1 200 OK"), "{method}: {body}");
        if method == "initialize" {
            self.session = head.lines().find_map(|line| line.strip_prefix("Mcp-Session-Id: ")).map(str::to_owned);
        }
        serde_json::from_str(&body).unwrap()
    }

    fn initialize(&mut self) -> Value {
        self.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }))
    }

    fn call(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let reply = self.mcp("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {reply}"));
        (reply["result"]["isError"] == true, serde_json::from_str(text).unwrap())
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

const A: &str = "0b7d0000-0000-4000-8000-000000000001";
const B: &str = "0b7d0000-0000-4000-8000-000000000002";

#[test]
#[ignore = "starts real Julia and R: cargo test -p endeavor-mcp --test e2e_r -- --ignored"]
fn an_r_notebook_through_the_runtime() {
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return;
    };
    let has_ember = Command::new("Rscript").args(["-e", "library(ember)"]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !has_ember {
        eprintln!("SKIPPED: no Rscript on the PATH with Ember installed.");
        return;
    }
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-r");
    let depot = work.join("depot");
    std::fs::create_dir_all(&depot).unwrap();
    let depot = match &app {
        Some(app) => format!("{}:{}:", depot.display(), app.join("depot").display()),
        None => format!("{}:", depot.display()),
    };
    if let Some(pid) = recorded_pid(&work.join("state")) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    let state = fresh(work.join("state"));
    let folder = fresh(work.join("project"));
    let mut cleanup = Cleanup { state: state.clone(), children: Vec::new() };
    std::fs::write(
        folder.join("growth.R"),
        format!("### An Ember notebook ###\n# /// environment\n# ///\n\n# %% id={A}\nx <- 20\n\n# %% id={B}\ny <- x + 1\n\n# /// cell order\n# {A}\n# {B}\n# ///\n"),
    )
    .unwrap();

    let mut serve = command(&["serve", "--folder", folder.to_str().unwrap()], &work, &julia, &depot)
        .env("ENDEAVOR_TEST_R_NOTEBOOKS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = lines(serve.stdout.take().unwrap());
    let err = lines(serve.stderr.take().unwrap());
    let printed = step("serve starts Julia", || {
        let mut printed = Vec::new();
        loop {
            match out.recv_timeout(Duration::from_secs(900)) {
                Ok(line) if line == "Press Ctrl-C to stop Julia." => break printed,
                Ok(line) => printed.push(line),
                Err(_) => panic!("serve printed {printed:?}; its log:\n{}", err.try_iter().collect::<Vec<_>>().join("\n")),
            }
        }
    });
    cleanup.children.push(serve);
    let link = printed.iter().find_map(|l| l.trim().strip_prefix("http://localhost:")).filter(|l| l.contains("/?token=")).unwrap();
    let (port, token) = link.split_once("/?token=").unwrap();
    let (port, token): (u16, String) = (port.parse().unwrap(), token.to_owned());
    let mut agent = Agent::new(port, &token);
    agent.initialize();

    let notebook = step("opening an R notebook starts R and Ember, in safe preview", || {
        let opened = agent.ok("open_notebook", json!({ "path": "growth.R" }));
        let notebook = opened["notebook_id"].as_str().unwrap().to_owned();
        assert_eq!((&opened["path"], &opened["execution_allowed"]), (&json!(folder.join("growth.R").display().to_string()), &json!(false)), "{opened}");
        assert_eq!(opened["browser_url"], json!(format!("http://localhost:{port}/ember/edit?id={notebook}&token={token}")));
        assert!(state.join("r.json").exists(), "R's adapter wrote its state");
        let listed = agent.ok("list_notebooks", json!({}));
        assert_eq!(listed.as_array().unwrap().iter().map(|nb| nb["notebook_id"].clone()).collect::<Vec<_>>(), [json!(notebook)], "{listed}");
        notebook
    });

    step("read, edit and run a cell; the output comes back", || {
        let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }));
        assert_eq!(order["cell_ids"], json!([A, B]), "{order}");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": A }));
        assert_eq!(read["code"], "x <- 20", "{read}");
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": A, "code": "x <- 41\n" }));
        agent.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
        agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": B }));
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": B, "code": "x + 1" }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": B, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": B }));
        assert_eq!(read["errored"], false, "{read}");
        assert!(read["output"].as_str().unwrap().contains("42"), "{read}");
    });

    step("the browser link reaches Ember's page through the port", || {
        let (status, head, _) = get(port, &format!("/ember/edit?id={notebook}&token={token}"), "");
        assert_eq!(status, "HTTP/1.1 303 See Other", "{head}");
        let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}"));
        let cookie = format!("Cookie: {}\r\n", set.split(';').next().unwrap());
        let (status, head, page) = get(port, &format!("/ember/edit?id={notebook}"), &format!("{cookie}Sec-Fetch-Site: none\r\n"));
        assert_eq!(status, "HTTP/1.1 200 OK", "{head}");
        assert!(!head.contains("ember_secret"), "Ember's cookie stays behind the port: {head}");
        assert!(page.to_lowercase().contains("ember"), "{}", &page[..page.len().min(300)]);
        assert!(get(port, &format!("/ember/edit?id={notebook}"), "").0.starts_with("HTTP/1.1 401"));
    });

    step("Ctrl-C stops Julia and R", || {
        let r_pid = std::fs::read_to_string(state.join("r.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).unwrap();
        let mut serve = cleanup.children.pop().unwrap();
        // SAFETY: plain syscall.
        unsafe { libc::kill(serve.id() as i32, libc::SIGINT) };
        assert!(serve.wait().unwrap().success());
        common::wait_for("R's adapter to end", || !common::pid_alive(r_pid as i32));
        assert!(!state.join("r.json").exists());
    });
}
