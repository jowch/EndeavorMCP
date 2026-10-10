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

use common::{find_julia, group_alive, pid_alive, wait_for};
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

    /// Each open notebook's file name, and whether it's this session's.
    fn this_session(&mut self) -> Vec<(String, bool)> {
        let listed = self.ok("list_notebooks", json!({}));
        let name = |nb: &Value| Path::new(nb["path"].as_str().unwrap()).file_name().unwrap().to_string_lossy().into_owned();
        let mut marked: Vec<(String, bool)> = listed.as_array().unwrap().iter().map(|nb| (name(nb), nb["this_session"] == true)).collect();
        marked.sort();
        marked
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

    let mut agent = Agent::new(port, &token);
    step("an agent with only the bearer token: no app, a session from initialize", || {
        let init = agent.initialize();
        let session = agent.session.as_deref().expect("initialize gives an Mcp-Session-Id");
        assert!(session.starts_with("mcp-") && session.len() == 36, "{session}");
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
        assert_eq!(created["browser_url"], json!(format!("http://localhost:{port}/edit?id={notebook}")), "no token: the agent is never given it");
        let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }));
        let last = order["cell_ids"].as_array().unwrap().last().unwrap().clone();
        let added = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "x = 21 * 2", "after_cell_id": last }));
        let cell = added["cell_id"].as_str().unwrap().to_owned();
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": cell, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
        assert_eq!((&read["output"], &read["errored"]), (&json!("42"), &json!(false)), "{read}");
        let status = agent.ok("session_status", json!({}));
        assert_eq!(status["browser_url"], json!(format!("http://localhost:{port}/")), "{status}");
        assert_eq!(agent.this_session(), vec![("analysis.jl".to_owned(), true)], "the notebook it made is this session's");
        notebook
    });

    step("a notebook from disk opens in safe preview, in another session", || {
        std::fs::copy(folder.join("analysis.jl"), folder.join("copy.jl")).unwrap();
        let (failed, refused) = agent.call("open_notebook", json!({ "path": "copy.jl" }));
        assert!(failed && refused["error"] == "one_notebook", "the first agent has its notebook: {refused}");
        let mut other = Agent::new(port, &token);
        other.initialize();
        assert_ne!(other.session, agent.session);
        let opened = other.ok("open_notebook", json!({ "path": "copy.jl" }));
        assert_eq!((&opened["path"], &opened["execution_allowed"]), (&json!(folder.join("copy.jl").display().to_string()), &json!(false)), "{opened}");
        assert_eq!(other.this_session(), vec![("analysis.jl".to_owned(), false), ("copy.jl".to_owned(), true)]);
        assert_eq!(agent.this_session(), vec![("analysis.jl".to_owned(), true), ("copy.jl".to_owned(), false)]);
    });

    step("a second agent opens the first one's notebook by path, joins it", || {
        let mut joiner = Agent::new(port, &token);
        joiner.initialize();
        let analysis = folder.join("analysis.jl").display().to_string();
        let joined = joiner.ok("open_notebook", json!({ "path": analysis, "run_notebook": true }));
        assert_eq!((&joined["notebook_id"], &joined["already_open"], &joined["ran"]), (&json!(notebook), &json!(true), &json!(false)), "{joined}");
        assert_eq!(joined["execution_allowed"], true, "as it was, not run again: {joined}");
        assert_eq!(joiner.this_session(), vec![("analysis.jl".to_owned(), true), ("copy.jl".to_owned(), false)]);
        assert_eq!(agent.this_session(), vec![("analysis.jl".to_owned(), true), ("copy.jl".to_owned(), false)], "still the first agent's");
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
        assert_eq!(opened["this_session"], false, "the user's, from the browser: {opened}");

        // What the endeavor-notebooks skill says of an open notebook the user names.
        let id = opened["notebook_id"].clone();
        let mut fresh = Agent::new(port, &token);
        fresh.initialize();
        let joined = fresh.ok("open_notebook", json!({ "path": path }));
        assert_eq!((&joined["notebook_id"], &joined["already_open"], &joined["execution_allowed"]), (&id, &json!(true), &json!(false)), "it joins, in safe preview as it was: {joined}");
        assert!(fresh.this_session().contains(&("from_browser.jl".to_owned(), true)), "and it is the session's own now");
        let last = fresh.ok("get_cell_order", json!({ "notebook_id": id }))["cell_ids"].as_array().unwrap().last().unwrap().clone();
        fresh.ok("read_cell", json!({ "notebook_id": id, "cell_id": last }));
        fresh.ok("add_cell", json!({ "notebook_id": id, "code": "y = 1", "after_cell_id": last }));
        let (failed, refused) = agent.call("add_cell", json!({ "notebook_id": id, "code": "y = 2", "after_cell_id": last }));
        assert!(failed && refused["error"] == "one_notebook", "a session with its own notebook is refused: {refused}");
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
        // Julia and its workers: nothing of the group runs (what is left may be zombies).
        assert!(!group_alive(core), "Julia's process group is gone");
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
    step("the stdio form answers the handshake at once and starts no Julia", || {
        send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {} } }));
        let init: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
        assert_eq!(init["result"]["instructions"], json!(said_standalone()), "{init}");
        send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } }));
        let reply: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
        assert_eq!(reply["result"]["content"][0]["text"], "[]", "{reply}");
        assert!(recorded_pid(&state).is_none(), "no runtime was started yet");
    });
    step("the first call that needs Julia starts it, and the stdio form says where the notebooks are", || {
        let started = Instant::now();
        loop {
            send(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "use_machine", "arguments": { "machine": "local" } } }));
            let reply: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(120)).unwrap()).unwrap();
            assert_eq!(reply["id"], 3);
            if reply["result"]["isError"] == false && reply["result"]["content"][0]["text"].as_str().unwrap().contains("\"state\":\"ready\"") {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(900), "{reply}");
        }
        let link = loop {
            let line = said.recv_timeout(Duration::from_secs(5)).expect("mcp says where the notebooks are");
            if let Some(link) = line.strip_prefix("Endeavor's notebooks: ") {
                break link.to_owned();
            }
        };
        assert!(link.starts_with("http://localhost:") && !link.contains("token"), "where the notebooks are, without the token: {link}");
    });
    step("a tool call goes through to the runtime", || {
        send(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } }));
        let reply: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(120)).unwrap()).unwrap();
        assert_eq!(reply["result"]["content"][0]["text"], "[]", "a new runtime has no notebooks open: {reply}");
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
        writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } })).unwrap();
        let reply: Value = serde_json::from_str(&replies.recv_timeout(Duration::from_secs(120)).unwrap()).unwrap();
        let listed: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let marked: Vec<(&str, &Value)> = listed.as_array().unwrap().iter().map(|nb| (nb["path"].as_str().unwrap(), &nb["this_session"])).collect();
        assert_eq!(marked, vec![(other.join("b.jl").to_str().unwrap(), &json!(true))], "the stdio form's session owns what it made: {listed}");
        assert_eq!(recorded_pid(&state), Some(core), "the same runtime");
        drop(stdin);
        assert!(mcp.wait().unwrap().success());
        let stopped = command(&["stop"], &work, &julia, &depot).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&stopped.stdout), format!("Stopped Julia (pid {core}).\n"));
        wait_for("the core to exit", || !pid_alive(core));
        assert!(!group_alive(core), "Julia's process group is gone");
    });
}

fn said_standalone() -> &'static str {
    "These tools edit and run live Pluto (Julia) notebooks without the Endeavor app: \
the user watches them in a web browser, on Pluto's own page, and there is no notebook pane next to this chat. \
`new_notebook` and `open_notebook` return `browser_url`: give it to the user. When the result has `opened_in_browser` true, the notebook should already be open in their browser; say so, and give the address in case it isn't. \
Endeavor's skills (or `notebook_guide`) and these tools' descriptions say where something holds only in the Endeavor app, \
such as the reference `app.md`: skip those parts. \
This server also has `list_machines`, `add_machine`, `use_machine` and `stop_machine`, which put this session's notebooks on a server or a Slurm cluster \
that the user reaches over ssh. `list_machines` only reads and is fine any time; call the other three only when the user asks."
}
