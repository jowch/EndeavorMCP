//! `endeavor serve` and `mcp` end to end, with real Julia, as a user
//! without the app runs them: serve in a terminal, the agent over HTTP with
//! the printed token, the browser through the printed link, then Ctrl-C; and
//! the stdio form for an agent on the same machine, then `stop`.
//!
//! Ignored by default, like e2e_julia (which says where Julia comes from):
//!
//!     cargo test -p endeavor-mcp --test e2e_serve -- --ignored --nocapture

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{find_julia, pid_alive, wait_for};
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

/// POST a JSON-RPC message to `/mcp` with the bearer token only, as an agent
/// configured from serve's output does: the status line and the body.
fn post(port: u16, token: &str, message: &Value) -> (String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
    let body = message.to_string();
    write!(
        socket,
        "POST /mcp HTTP/1.0\r\nHost: localhost:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.lines().next().unwrap_or_default().to_owned(), body.to_owned())
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
}

impl Agent {
    fn mcp(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let (status, body) = post(self.port, &self.token, &json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params }));
        assert_eq!(status, "HTTP/1.1 200 OK", "{method}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.mcp("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {reply}"));
        assert_eq!(reply["result"]["isError"], false, "{name}({arguments}): {text}");
        serde_json::from_str(text).unwrap()
    }
}

fn step<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    eprintln!("── {name}");
    let result = f();
    eprintln!("   {:.1}s", started.elapsed().as_secs_f64());
    result
}

#[test]
#[ignore = "starts real Julia twice, about a minute: cargo test -p endeavor-mcp --test e2e_serve -- --ignored"]
fn serve_and_mcp_without_the_app() {
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return;
    };
    eprintln!("julia: {}", julia.display());
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-serve");
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

    let mut serve = command(&["serve", "--folder", folder.to_str().unwrap()], &work, &julia, &depot)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = lines(serve.stdout.take().unwrap());
    let err = lines(serve.stderr.take().unwrap());
    let printed = step("serve starts Julia and prints how to connect", || {
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
    let text = printed.join("\n");
    assert!(text.contains(&format!("New notebooks go in {}.", folder.display())), "{text}");
    assert!(text.contains(&format!("claude mcp add --transport http endeavor http://localhost:{port}/mcp --header \"Authorization: Bearer {token}\"")), "{text}");
    assert!(text.contains(&format!("    ssh -L {port}:localhost:{port} ")), "{text}");
    let core = recorded_pid(&state).expect("runtime.json names the core");
    let marker = work.join("cache/endeavor/serve").join(endeavor_mcp::embedded::RUNTIME_VERSION).join("in-use");
    let marker = std::fs::OpenOptions::new().write(true).open(&marker).unwrap();
    assert!(marker.try_lock().is_err(), "the core holds a lease on its runtime's folder, which another version's unpack then keeps");
    drop(marker);

    let mut agent = Agent { port, token: token.clone(), id: 0 };
    step("an agent with only the bearer token: no session, no app", || {
        let init = agent.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }));
        let instructions = init["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("call `notebook_guide` once") && instructions.contains("the user watches them in a web browser"), "{instructions}");
        let tools = agent.mcp("tools/list", json!({}));
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"notebook_guide") && names.contains(&"execute_cell") && !names.contains(&"run_shell"), "{names:?}");
    });

    let notebook = step("new notebook in serve's folder, run a cell, read its output", || {
        let created = agent.ok("new_notebook", json!({ "path": "analysis.jl" }));
        let notebook = created["notebook_id"].as_str().unwrap().to_owned();
        assert_eq!(created["path"], json!(folder.join("analysis.jl").display().to_string()), "{created}");
        assert_eq!(created["browser_url"], json!(format!("http://localhost:{port}/edit?id={notebook}&token={token}")));
        let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }));
        let last = order["cell_ids"].as_array().unwrap().last().unwrap().clone();
        let added = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "x = 21 * 2", "after_cell_id": last }));
        let cell = added["cell_id"].as_str().unwrap().to_owned();
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": cell, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
        assert_eq!((&read["output"], &read["errored"]), (&json!("42"), &json!(false)), "{read}");
        let status = agent.ok("pluto_session_status", json!({}));
        assert_eq!(status["browser_url"], json!(format!("http://localhost:{port}/?token={token}")), "{status}");
        notebook
    });

    step("a notebook from disk opens in safe preview", || {
        std::fs::copy(folder.join("analysis.jl"), folder.join("copy.jl")).unwrap();
        let opened = agent.ok("open_notebook", json!({ "path": "copy.jl" }));
        assert_eq!((&opened["path"], &opened["execution_allowed"]), (&json!(folder.join("copy.jl").display().to_string()), &json!(false)), "{opened}");
    });

    step("the browser link sets the cookie and opens Pluto's page", || {
        let (status, head, _) = get(port, &format!("/edit?id={notebook}&token={token}"), "");
        assert_eq!(status, "HTTP/1.1 303 See Other", "{head}");
        let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}"));
        let cookie = format!("Cookie: {}\r\n", set.split(';').next().unwrap());
        let (status, head, page) = get(port, &format!("/edit?id={notebook}"), &format!("{cookie}Sec-Fetch-Site: none\r\n"));
        assert_eq!(status, "HTTP/1.1 200 OK", "{head}");
        assert!(page.contains("Pluto"), "{}", &page[..page.len().min(300)]);
        assert!(get(port, "/", "").0.starts_with("HTTP/1.1 401"));

        std::fs::copy(folder.join("analysis.jl"), folder.join("from_browser.jl")).unwrap();
        let path = folder.join("from_browser.jl").display().to_string();
        let (status, head, _) = get(port, &format!("/open?path={path}"), &format!("{cookie}Sec-Fetch-Site: same-origin\r\n"));
        assert_eq!(status, "HTTP/1.1 302 Moved Temporarily", "Pluto's start page opening a file: {head}");
        let listed = agent.ok("list_notebooks", json!({}));
        let opened = listed.as_array().unwrap().iter().find(|nb| nb["path"] == json!(path)).cloned().unwrap();
        assert_eq!(opened["execution_allowed"], false, "safe preview: {opened}");
    });

    step("Ctrl-C stops Julia", || {
        let serve = cleanup.children.pop().unwrap();
        // SAFETY: plain syscall.
        unsafe { libc::kill(serve.id() as i32, libc::SIGINT) };
        let mut serve = serve;
        let status = serve.wait().unwrap();
        assert!(status.success(), "{status}");
        let said: Vec<String> = err.try_iter().collect();
        assert!(said.ends_with(&["Stopping Julia…".to_owned(), "Stopped.".to_owned()]), "{said:?}");
        assert!(!pid_alive(core), "the core is gone");
        // SAFETY: signal 0 only checks; ESRCH means the group, Julia and its workers, is gone.
        assert_eq!(unsafe { libc::kill(-core, 0) }, -1, "Julia's process group is gone");
        assert!(!state.join("runtime.json").exists());
    });

    let mut mcp = command(&["mcp", "--folder", folder.to_str().unwrap(), "--skills", "plugin"], &work, &julia, &depot)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let replies = lines(mcp.stdout.take().unwrap());
    let said = lines(mcp.stderr.take().unwrap());
    let mut stdin = mcp.stdin.take().unwrap();
    cleanup.children.push(mcp);
    let mut send = |message: Value| writeln!(stdin, "{message}").unwrap();
    step("the stdio form answers the handshake at once and starts Julia behind it", || {
        send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {} } }));
        let init: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
        assert_eq!(init["result"]["instructions"], json!(said_standalone()), "{init}");
        send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        let link = loop {
            let line = said.recv_timeout(Duration::from_secs(900)).expect("mcp says where the notebooks are");
            if let Some(link) = line.strip_prefix("Endeavor's notebooks: ") {
                break link.to_owned();
            }
        };
        assert!(link.starts_with("http://localhost:") && link.contains("/?token="), "{link}");
    });
    step("a tool call goes through to the runtime", || {
        let started = Instant::now();
        loop {
            send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } }));
            let reply: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(120)).unwrap()).unwrap();
            assert_eq!(reply["id"], 2);
            if reply["result"]["isError"] == false {
                assert_eq!(reply["result"]["content"][0]["text"], "[]", "a new runtime has no notebooks open: {reply}");
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(900), "{reply}");
        }
    });
    let core = recorded_pid(&state).unwrap();
    step("the runtime outlives the agent, and stop ends it", || {
        let mut mcp = cleanup.children.pop().unwrap();
        drop(stdin);
        assert!(mcp.wait().unwrap().success());
        assert!(pid_alive(core), "still running after the agent went");

        // Another agent, in another project, uses the same runtime with its own folder.
        let other = fresh(work.join("other"));
        let mut mcp = command(&["mcp", "--folder", other.to_str().unwrap()], &work, &julia, &depot)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let replies = lines(mcp.stdout.take().unwrap());
        let mut stdin = mcp.stdin.take().unwrap();
        writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "new_notebook", "arguments": { "path": "b.jl" } } })).unwrap();
        let reply: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(120)).unwrap()).unwrap();
        let created: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(created["path"], json!(other.join("b.jl").display().to_string()), "{reply}");
        assert_eq!(recorded_pid(&state), Some(core), "the same runtime");
        drop(stdin);
        assert!(mcp.wait().unwrap().success());
        let stopped = command(&["stop"], &work, &julia, &depot).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&stopped.stdout), format!("Stopped Julia (pid {core}).\n"));
        wait_for("the core to exit", || !pid_alive(core));
        // SAFETY: as above.
        assert_eq!(unsafe { libc::kill(-core, 0) }, -1, "Julia's process group is gone");
    });
}

fn said_standalone() -> &'static str {
    "These tools edit and run live Pluto (Julia) notebooks without the Endeavor app: \
the user watches them in a web browser, on Pluto's own page, and there is no notebook pane next to this chat. \
Where Endeavor's notes on these tools say \"in the app\" or \"without the app\", follow the parts for working without the app."
}
