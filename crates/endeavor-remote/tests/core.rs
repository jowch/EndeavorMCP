//! `endeavor-remote core` with a stand-in Julia (see `common`): it starts it,
//! writes `runtime.json` once it's ready, passes every request on its bridge
//! port through, streams as they're written, and lives and dies with it.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::*;

struct Core {
    process: Child,
    port: u16,
    julia_pid: i32,
}

impl Core {
    /// Start the core in `dir` and wait for its `runtime.json`.
    fn start(dir: &Path, bridge: &FakeBridge) -> Core {
        Core::start_with_home(dir, bridge, &std::env::var("HOME").unwrap())
    }

    fn start_with_home(dir: &Path, bridge: &FakeBridge, home: &str) -> Core {
        let julia = serving_julia(dir, bridge);
        let process = Command::new(env!("CARGO_BIN_EXE_endeavor-remote"))
            .env("HOME", home)
            .arg("core")
            .arg("--state-dir")
            .arg(dir)
            .arg("--julia")
            .arg(&julia)
            .args(["--runtime", "/opt/runtime", "--depot", "/opt/depot:"])
            .env("ENDEAVOR_TOKEN", TOKEN)
            .env("ENDEAVOR_LAUNCHER", "process")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        wait_for("runtime.json", || dir.join("runtime.json").exists());
        let state = read_json(&dir.join("runtime.json"));
        let julia_pid = read_json(&dir.join("julia.json"))["pid"].as_i64().unwrap() as i32;
        Core { port: state["mcp_port"].as_u64().unwrap() as u16, process, julia_pid }
    }

    fn connect(&self) -> TcpStream {
        let socket = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        socket
    }

    fn exits(&mut self) -> std::process::ExitStatus {
        let mut status = None;
        wait_for("the core to exit", || {
            status = self.process.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap()
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(self.julia_pid, libc::SIGKILL) };
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Read one response: its head, and its body undone from chunks or by length.
fn response(reader: &mut impl BufRead) -> (String, Vec<(String, String)>, String) {
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let Some((name, value)) = line.trim_end().split_once(": ") else { break };
        headers.push((name.to_ascii_lowercase(), value.to_owned()));
    }
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    let mut body = Vec::new();
    if header("transfer-encoding").as_deref() == Some("chunked") {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).unwrap();
            let size = usize::from_str_radix(size.trim_end(), 16).unwrap();
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(length) = header("content-length") {
        body = vec![0; length.parse().unwrap()];
        reader.read_exact(&mut body).unwrap();
    } else {
        reader.read_to_end(&mut body).unwrap();
    }
    (status.trim_end().to_owned(), headers, String::from_utf8(body).unwrap())
}

/// Read a stream until `text` has come.
fn read_until(reader: &mut impl BufRead, text: &str) -> String {
    let mut got = String::new();
    while !got.contains(text) {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "stream ended before {text:?}; got {got:?}");
        got.push_str(&line);
    }
    got
}

#[test]
fn starts_julia_and_writes_its_own_runtime_json() {
    let dir = state_dir("core-state");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let state = read_json(&dir.join("runtime.json"));
    assert_eq!(state["pid"].as_i64(), Some(core.process.id() as i64), "the core's pid");
    assert_ne!(core.port, bridge.port, "the core's own bridge port");
    assert_eq!(state["pluto_port"].as_u64(), Some(bridge.pluto_port as u64), "Pluto is still Julia's");
    assert_eq!((state["token"].as_str(), state["pluto_secret"].as_str(), state["launcher"].as_str()), (Some(TOKEN), Some("s3cret"), Some("process")));
    let mode = std::fs::metadata(dir.join("runtime.json")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let args = std::fs::read_to_string(dir.join("julia.args")).unwrap();
    let args: Vec<&str> = args.split_whitespace().collect();
    assert_eq!(&args[..3], ["--color=no", "--project=/opt/runtime", "/opt/runtime/boot.jl"]);
    let ports: Vec<u16> = args[3..].iter().map(|p| p.parse().unwrap()).collect();
    assert!(ports.len() == 2 && ports[0] != ports[1] && !ports.contains(&core.port), "{ports:?}");

    let ping = bridge.seen().into_iter().find(|s| s.line == "POST /call HTTP/1.0").expect("checked Julia's bridge answers");
    assert!(String::from_utf8_lossy(&ping.body).contains("ping"));
}

#[test]
fn forwards_calls_with_their_headers_and_host_rewritten() {
    let dir = state_dir("core-call");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let body = r#"{"jsonrpc":"2.0","id":7,"method":"endeavor/stop_notebook","params":{"path":"/n/a.jl"}}"#;
    write!(
        socket,
        "POST /call HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nX-Endeavor-Host: labbox3\r\nContent-Length: {}\r\n\r\n{body}",
        core.port,
        body.len()
    )
    .unwrap();
    let (status, headers, reply) = response(&mut reader);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains(&("transfer-encoding".into(), "chunked".into())), "Julia's framing passes through");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["result"]["body"].as_str(), Some(body));

    let seen = bridge.seen().into_iter().find(|s| s.line == "POST /call HTTP/1.1").unwrap();
    assert_eq!(seen.header("Host"), Some(format!("127.0.0.1:{}", bridge.port).as_str()));
    assert_eq!(seen.header("Authorization"), Some(format!("Bearer {TOKEN}").as_str()));
    assert_eq!(seen.header("Content-Type"), Some("application/json"));
    assert_eq!(seen.header("X-Endeavor-Host"), Some("labbox3"));

    // A chunked call, which the core reads whole to see its method, reaches Julia whole.
    write!(socket, "POST /call HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
    write!(socket, "{:x}\r\n{}\r\n", 20, &body[..20]).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    write!(socket, "{:x}\r\n{}\r\n0\r\n\r\n", body.len() - 20, &body[20..]).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&response(&mut reader).2).unwrap();
    assert_eq!(reply["result"]["body"].as_str(), Some(body));

    // The same connection carries the next request.
    write!(socket, "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", core.port).unwrap();
    assert_eq!(response(&mut reader), ("HTTP/1.1 200 OK".into(), vec![("content-length".into(), "2".into())], "ok".into()));
    write!(socket, "GET /nope HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    assert_eq!(response(&mut reader).0, "HTTP/1.1 404 Not Found");

    // HTTP/1.0, as the helper's own calls are: the reply runs to the end of the connection.
    let mut socket = core.connect();
    write!(socket, "POST /call HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}").unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{reply}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(body).unwrap()["result"]["said"], "POST /call HTTP/1.0");
}

#[test]
fn passes_request_bodies_intact() {
    let dir = state_dir("core-body");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let body: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    write!(socket, "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
    socket.write_all(&body).unwrap();
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert_eq!(status, "HTTP/1.1 200 OK\r\n");
    let mut line = String::new();
    while line != "\r\n" {
        line.clear();
        reader.read_line(&mut line).unwrap();
    }
    let mut back = vec![0; body.len()];
    reader.read_exact(&mut back).unwrap();
    assert!(back == body, "a 1 MB body comes back unchanged");

    // A chunked body, sent in pieces.
    write!(socket, "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
    socket.write_all(b"6\r\nhello \r\n").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    socket.write_all(b"5\r\nworld\r\n0\r\n\r\n").unwrap();
    assert_eq!(response(&mut reader).2, "hello world");
}

#[test]
fn streams_events_as_written_and_closes_with_either_side() {
    let dir = state_dir("core-events");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    write!(socket, "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    bridge.event("first");
    assert!(read_until(&mut reader, "data: first").starts_with("HTTP/1.1 200 OK\r\n"));
    bridge.event("second");
    read_until(&mut reader, "data: second");

    // The client going away closes Julia's end at once, with nothing written.
    drop((socket, reader));
    assert!(bridge.events_closed(), "the core closed the stream to Julia");

    // Julia's end closing mid-stream closes the client's.
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    write!(socket, "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    bridge.event("third");
    read_until(&mut reader, "data: third");
    bridge.drop_events();
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
}

/// An agent session's MCP connection: its event stream and its session id.
struct Session {
    events: BufReader<TcpStream>,
    id: String,
}

impl Session {
    fn open(core: &Core) -> Session {
        let mut stream = core.connect();
        write!(stream, "GET /sse HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nAccept: text/event-stream\r\n\r\n", core.port).unwrap();
        let mut events = BufReader::new(stream);
        let head = read_until(&mut events, "\r\n\r\n");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Type: text/event-stream\r\n") && head.contains("Transfer-Encoding: chunked\r\n"), "{head}");
        let endpoint = read_until(&mut events, "\n\n");
        let id = endpoint.split("data: /message?sessionId=").nth(1).expect(&endpoint).trim().to_owned();
        Session { events, id }
    }

    /// The next reply on the stream, as the core wrote it.
    fn reply(&mut self) -> String {
        let text = read_until(&mut self.events, "\n\n");
        let data = text.split("event: message\ndata: ").nth(1).unwrap_or_else(|| panic!("no message in {text:?}"));
        data.trim_end().to_owned()
    }
}

/// POST one message to a session as the agent does: its status and body.
fn post(core: &Core, session: &str, message: &str, caller: &[(&str, &str)]) -> (String, String) {
    let mut socket = core.connect();
    let headers: String = caller.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
    write!(
        socket,
        "POST /message?sessionId={session} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\n\r\n{message}",
        core.port,
        message.len()
    )
    .unwrap();
    let (status, _, body) = response(&mut BufReader::new(socket));
    (status, body)
}

#[test]
fn serves_the_agents_mcp_sessions_and_asks_julia_with_the_caller() {
    let dir = state_dir("core-mcp");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut session = Session::open(&core);
    let caller = [("X-Endeavor-Session", "7"), ("X-Endeavor-Host", "gpu-box")];

    let message = r#"{"jsonrpc":"2.0","id":1,"method":"resources/list","params":{}}"#;
    assert_eq!(post(&core, &session.id, message, &caller), ("HTTP/1.1 202 Accepted".into(), String::new()));
    assert_eq!(session.reply(), r#"{"id":1,"jsonrpc":"2.0","result":{"host":"gpu-box","method":"resources/list","owner":"7"}}"#);
    let asked: Vec<_> = bridge.seen().into_iter().filter(|s| s.line == "POST /dispatch HTTP/1.1").collect();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].body, message.as_bytes(), "the message as the agent sent it");
    assert_eq!(asked[0].header("Authorization"), Some(format!("Bearer {TOKEN}").as_str()));

    // Answered here: ping, and notifications (which get no reply).
    assert_eq!(post(&core, &session.id, r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#, &caller).0, "HTTP/1.1 202 Accepted");
    assert_eq!(session.reply(), r#"{"id":"p","jsonrpc":"2.0","result":{}}"#);
    assert_eq!(post(&core, &session.id, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &caller).0, "HTTP/1.1 202 Accepted");
    assert_eq!(post(&core, &session.id, r#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#, &[]).0, "HTTP/1.1 202 Accepted");
    assert_eq!(session.reply(), r#"{"id":2,"jsonrpc":"2.0","result":{"host":"","method":"initialize","owner":""}}"#, "the app's own calls have no caller");
    assert_eq!(bridge.seen().iter().filter(|s| s.line == "POST /dispatch HTTP/1.1").count(), 2);

    assert_eq!(post(&core, "nope", message, &caller), ("HTTP/1.1 404 Not Found".into(), r#"{"error":"Session not found"}"#.into()));
    assert_eq!(post(&core, &session.id, "{nope", &caller), ("HTTP/1.1 400 Bad Request".into(), r#"{"error":"Invalid JSON"}"#.into()));
    assert_eq!(post(&core, &session.id, "[1]", &caller).0, "HTTP/1.1 400 Bad Request");
}

#[test]
fn keeps_concurrent_sessions_apart() {
    let dir = state_dir("core-sessions");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut sessions = [Session::open(&core), Session::open(&core)];
    assert_ne!(sessions[0].id, sessions[1].id);
    std::thread::scope(|scope| {
        for (n, session) in sessions.iter().enumerate() {
            for i in 0..10 {
                let (core, id) = (&core, session.id.clone());
                scope.spawn(move || {
                    let message = format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"tools/call"}}"#);
                    assert_eq!(post(core, &id, &message, &[("X-Endeavor-Session", &n.to_string())]).0, "HTTP/1.1 202 Accepted");
                });
            }
        }
    });
    for (n, session) in sessions.iter_mut().enumerate() {
        let mut ids: Vec<i64> = (0..10)
            .map(|_| {
                let reply: serde_json::Value = serde_json::from_str(&session.reply()).unwrap();
                assert_eq!(reply["result"]["owner"], n.to_string());
                reply["id"].as_i64().unwrap()
            })
            .collect();
        ids.sort();
        assert_eq!(ids, (0..10).collect::<Vec<_>>());
    }
}

/// One of the app's `/call`s, as the app makes them: the reply's body.
fn app_call(core: &Core, body: &str) -> String {
    let mut socket = core.connect();
    write!(socket, "POST /call HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", core.port, body.len()).unwrap();
    let (status, _, reply) = response(&mut BufReader::new(socket));
    assert_eq!(status, "HTTP/1.1 200 OK");
    reply
}

/// A failed tool call's reply, as the agent saw it from Julia.
fn tool_error(id: u32, kind: &str, message: &str) -> String {
    let text = serde_json::Value::from(format!(r#"{{"error":"{kind}","message":{}}}"#, serde_json::Value::from(message)));
    format!(r#"{{"id":{id},"jsonrpc":"2.0","result":{{"content":[{{"text":{text},"type":"text"}}],"isError":true}}}}"#)
}

#[test]
fn plan_mode_refuses_a_sessions_writes_and_runs_and_host_tools_need_a_server() {
    let dir = state_dir("core-policy");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut session = Session::open(&core);
    let tool = |id: u32, name: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{{"notebook_id":"n1"}}}}}}"#);
    let asked = || bridge.seen().iter().filter(|s| s.line == "POST /dispatch HTTP/1.1").count();
    let told = || bridge.seen().iter().filter(|s| String::from_utf8_lossy(&s.body).contains("endeavor/tool_called")).count();
    let plan_edit = "Plan mode is read-only: `edit_cell` would change or run the notebook. Finish the plan; the user switches modes to carry it out.";

    let set = r#"{"jsonrpc":"2.0","id":5,"method":"endeavor/set_policy","params":{"owner":"7","policy":"plan"}}"#;
    assert_eq!(app_call(&core, set), r#"{"id":5,"jsonrpc":"2.0","result":{}}"#);
    assert!(!bridge.seen().iter().any(|s| String::from_utf8_lossy(&s.body).contains("set_policy")), "the core keeps policies");

    let seven = [("X-Endeavor-Session", "7")];
    post(&core, &session.id, &tool(1, "edit_cell"), &seven);
    assert_eq!(session.reply(), tool_error(1, "plan_mode", plan_edit));
    assert_eq!((asked(), told()), (0, 1), "refused here; Julia hears of the call");
    let called = bridge.seen().into_iter().find(|s| String::from_utf8_lossy(&s.body).contains("endeavor/tool_called")).unwrap();
    let called: serde_json::Value = serde_json::from_slice(&called.body).unwrap();
    assert_eq!(called["params"]["arguments"], serde_json::json!({ "notebook_id": "n1" }));

    // Reads pass, and so do other sessions' writes and the app's own.
    post(&core, &session.id, &tool(2, "read_cell"), &seven);
    assert!(session.reply().contains(r#""method":"tools/call","owner":"7""#));
    post(&core, &session.id, &tool(3, "edit_cell"), &[("X-Endeavor-Session", "8")]);
    assert!(session.reply().contains(r#""owner":"8""#));
    assert!(app_call(&core, &tool(4, "edit_cell")).contains(r#""method":"tools/call","owner":"""#));
    assert_eq!(asked(), 3);

    // Host tools: plan mode refuses run_shell too; without a server, none run.
    let on_server = [("X-Endeavor-Session", "7"), ("X-Endeavor-Host", "gpu-box")];
    post(&core, &session.id, &tool(5, "run_shell"), &on_server);
    let plan_shell = "Plan mode is read-only: `run_shell` would run a command on the server. Finish the plan; the user switches modes to carry it out.";
    assert_eq!(session.reply(), tool_error(5, "plan_mode", plan_shell));
    let not_here = |tool: &str| format!("`{tool}` is only for sessions on a server. This session runs on this Mac: use your own file and shell tools.");
    post(&core, &session.id, &tool(6, "run_shell"), &seven);
    assert_eq!(session.reply(), tool_error(6, "host_tools", &not_here("run_shell")), "the host check comes first");
    assert_eq!(app_call(&core, &tool(7, "list_folder")), tool_error(7, "host_tools", &not_here("list_folder")));
    assert_eq!((asked(), told()), (3, 4));

    // A call with arguments Julia can't read goes to Julia, which says so its own way.
    post(&core, &session.id, r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"edit_cell","arguments":null}}"#, &seven);
    session.reply();
    assert_eq!(asked(), 4);

    assert_eq!(app_call(&core, &set.replace("plan", "ask")), r#"{"id":5,"jsonrpc":"2.0","result":{}}"#);
    post(&core, &session.id, &tool(9, "edit_cell"), &seven);
    assert!(session.reply().contains(r#""owner":"7""#));

    // Every tool says whether it only reads, for Claude Code's own plan mode.
    post(&core, &session.id, r#"{"jsonrpc":"2.0","id":10,"method":"tools/list"}"#, &seven);
    assert_eq!(
        session.reply(),
        r#"{"id":10,"jsonrpc":"2.0","result":{"tools":[{"annotations":{"readOnlyHint":false},"name":"edit_cell"},{"annotations":{"readOnlyHint":true},"name":"read_cell"}]}}"#
    );
}

/// A folder of its own for a test.
fn temp_folder(name: &str) -> std::path::PathBuf {
    let dir = state_dir(name);
    dir.canonicalize().unwrap()
}

/// A session on a server, calling host tools.
struct OnServer<'a> {
    core: &'a Core,
    session: Session,
    next: u32,
}

impl OnServer<'_> {
    /// A tool call's result (`Ok`) or error (`Err`), as the agent reads them.
    fn call(&mut self, owner: &str, name: &str, arguments: serde_json::Value) -> Result<serde_json::Value, serde_json::Value> {
        self.next += 1;
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": self.next, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        let caller = [("X-Endeavor-Session", owner), ("X-Endeavor-Host", "gpu-box")];
        assert_eq!(post(self.core, &self.session.id, &message.to_string(), &caller).0, "HTTP/1.1 202 Accepted");
        let reply: serde_json::Value = serde_json::from_str(&self.session.reply()).unwrap();
        assert_eq!(reply["id"], self.next);
        let body: serde_json::Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        if reply["result"]["isError"] == true { Err(body) } else { Ok(body) }
    }

    fn run(&mut self, arguments: serde_json::Value) -> serde_json::Value {
        self.call("", "run_shell", arguments).unwrap()
    }
}

#[test]
fn host_tools_are_listed_for_sessions_on_a_server_and_run_here() {
    let dir = state_dir("core-host-list");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut session = Session::open(&core);
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    post(&core, &session.id, list, &[("X-Endeavor-Host", "gpu-box")]);
    let reply: serde_json::Value = serde_json::from_str(&session.reply()).unwrap();
    let tools = reply["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["edit_cell", "read_cell", "list_folder", "read_file", "run_shell"], "Julia's tools, then the host tools");
    let hints: Vec<_> = tools.iter().map(|t| t["annotations"]["readOnlyHint"].as_bool().unwrap()).collect();
    assert_eq!(hints, [false, true, true, true, false]);
    assert_eq!(tools[4]["inputSchema"]["required"], serde_json::json!(["command"]));
    post(&core, &session.id, list, &[]);
    assert!(!session.reply().contains("list_folder"), "not on this Mac");

    // Answered here, and Julia hears of each call.
    let mut server = OnServer { core: &core, session, next: 1 };
    let told = || bridge.seen().iter().filter(|s| String::from_utf8_lossy(&s.body).contains("endeavor/tool_called")).count();
    server.run(serde_json::json!({ "command": "true" }));
    assert_eq!(told(), 1);
    assert!(!bridge.seen().iter().any(|s| s.line.starts_with("POST /dispatch") && String::from_utf8_lossy(&s.body).contains("run_shell")));
}

#[test]
fn list_folder_and_read_file_read_the_server() {
    let dir = state_dir("core-host-files");
    let bridge = FakeBridge::start(&dir);
    let home = temp_folder("core-host-files-home");
    std::fs::create_dir(home.join("b_dir")).unwrap();
    std::fs::create_dir(home.join("z_dir")).unwrap();
    std::fs::write(home.join(".env"), "KEY=1\n").unwrap();
    std::fs::write(home.join("a.txt"), "hello\n").unwrap();
    let core = Core::start_with_home(&dir, &bridge, home.to_str().unwrap());
    let mut server = OnServer { core: &core, session: Session::open(&core), next: 0 };
    let home_path = home.to_str().unwrap();

    for path in ["~", "", "  "] {
        let listed = server.call("", "list_folder", serde_json::json!({ "path": path })).unwrap();
        assert_eq!(listed["path"], home_path);
        let names: Vec<_> = listed["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["b_dir", "z_dir", ".env", "a.txt"]);
        let kinds: Vec<_> = listed["entries"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["dir", "dir", "file", "file"]);
        assert_eq!(listed["entries"][3]["size"], 6);
        assert!(listed["entries"][0].get("size").is_none());
        assert!(listed["entries"][3]["modified"].is_i64());
        assert_eq!((&listed["total"], &listed["truncated"]), (&serde_json::json!(4), &serde_json::json!(false)));
    }
    assert_eq!(server.call("", "list_folder", serde_json::json!({})).unwrap()["path"], home_path);
    let b_dir = format!("{home_path}/b_dir");
    for path in ["~/b_dir", "b_dir", "~/z_dir/../b_dir"] {
        assert_eq!(server.call("", "list_folder", serde_json::json!({ "path": path })).unwrap()["path"], b_dir.as_str(), "{path}");
    }
    let error = |kind: &str, message: String| serde_json::json!({ "error": kind, "message": message });
    assert_eq!(server.call("", "list_folder", serde_json::json!({ "path": "~/missing" })), Err(error("not_found", format!("No folder at {home_path}/missing"))));
    assert_eq!(server.call("", "list_folder", serde_json::json!({ "path": "~/a.txt" })), Err(error("not_a_folder", format!("{home_path}/a.txt is a file, not a folder"))));
    assert_eq!(server.call("", "list_folder", serde_json::json!({ "path": "~bob" })), Err(error("tool_error", "ArgumentError: ~user tilde expansion not yet implemented".into())));

    let many = temp_folder("core-host-many");
    for i in 1..=1005 {
        std::fs::write(many.join(format!("f{i}")), "").unwrap();
    }
    let crowded = server.call("", "list_folder", serde_json::json!({ "path": many })).unwrap();
    assert_eq!((crowded["entries"].as_array().unwrap().len(), &crowded["total"], &crowded["truncated"]), (1000, &serde_json::json!(1005), &serde_json::json!(true)));
    assert_eq!(crowded["entries"][1]["name"], "f10", "sorted by name");

    let files = temp_folder("core-host-read");
    let lines = files.join("lines.txt");
    std::fs::write(&lines, (1..=10).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
    let part = server.call("", "read_file", serde_json::json!({ "path": lines, "offset": 3, "limit": 2 })).unwrap();
    assert_eq!(part["text"], "     3\tline 3\n     4\tline 4\n");
    assert_eq!((&part["start_line"], &part["end_line"], &part["total_lines"], &part["truncated"]), (&serde_json::json!(3), &serde_json::json!(4), &serde_json::json!(10), &serde_json::json!(true)));
    let whole = server.call("", "read_file", serde_json::json!({ "path": lines })).unwrap();
    assert_eq!((&whole["end_line"], &whole["total_lines"], &whole["truncated"]), (&serde_json::json!(10), &serde_json::json!(10), &serde_json::json!(false)));
    let tail = server.call("", "read_file", serde_json::json!({ "path": lines, "offset": 9.0 })).unwrap();
    assert_eq!((&tail["text"], &tail["truncated"]), (&serde_json::json!("     9\tline 9\n    10\tline 10\n"), &serde_json::json!(false)));
    let invalid = |message: &str| Err(error("invalid_argument", message.into()));
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": lines, "offset": 0 })), invalid("offset is the first line to read, 1 or more"));
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": lines, "limit": 0 })), invalid("limit must be 1 or more"));
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": lines, "limit": 1.5 })), invalid("limit must be a whole number"));
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": 5 })), invalid("path must be a string"));
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": "~/b_dir" })), Err(error("not_a_file", format!("{b_dir} is a folder; use list_folder"))));
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": "nope" })), Err(error("not_found", format!("No file at {home_path}/nope"))));

    let crlf = files.join("crlf.txt");
    std::fs::write(&crlf, "one\r\ntwo\r\nthree").unwrap();
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": crlf })).unwrap()["text"], "     1\tone\n     2\ttwo\n     3\tthree\n");

    let long = files.join("long.txt");
    std::fs::write(&long, format!("{}\nshort\n", "é".repeat(3000))).unwrap();
    let long = server.call("", "read_file", serde_json::json!({ "path": long })).unwrap();
    let text = long["text"].as_str().unwrap();
    assert!(text.starts_with(&format!("     1\t{} [line cut at 2000 characters]\n", "é".repeat(2000))), "cut at 2000 characters, not bytes");
    assert!(text.ends_with("     2\tshort\n"));
    assert_eq!(long["truncated"], true);

    let big = files.join("big.txt");
    std::fs::write(&big, format!("{}\n", "y".repeat(999)).repeat(1000)).unwrap();
    let big = server.call("", "read_file", serde_json::json!({ "path": big })).unwrap();
    assert_eq!((&big["total_lines"], &big["truncated"]), (&serde_json::json!(1000), &serde_json::json!(true)));
    assert!(big["end_line"].as_i64().unwrap() < 1000);
    let size = big["text"].as_str().unwrap().len();
    assert!((256 * 1024..256 * 1024 + 1100).contains(&size), "{size}");

    let binary = files.join("data.bin");
    std::fs::write(&binary, [0x41, 0x00, 0x42]).unwrap();
    let message = format!("{} is a binary file (it has NUL bytes); read_file only reads text", binary.display());
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": binary })), Err(error("binary_file", message)));

    let bad = files.join("bad.txt");
    std::fs::write(&bad, [0x61, 0xe2, 0x82, 0x62, 0x0a, 0xff, 0xc0, 0x80, 0x7f]).unwrap();
    assert_eq!(server.call("", "read_file", serde_json::json!({ "path": bad })).unwrap()["text"], "     1\ta\u{fffd}b\n     2\t\u{fffd}\u{fffd}\u{7f}\n");

    let locked = files.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let denied = server.call("", "list_folder", serde_json::json!({ "path": locked }));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(denied, Err(error("tool_error", format!("IOError: readdir(\"{}\"): permission denied (EACCES)", locked.display()))));
}

#[test]
fn run_shell_runs_in_the_login_shell_and_keeps_to_its_limits() {
    let dir = state_dir("core-host-shell");
    let bridge = FakeBridge::start(&dir);
    let home = temp_folder("core-host-shell-home");
    let core = Core::start_with_home(&dir, &bridge, home.to_str().unwrap());
    let mut server = OnServer { core: &core, session: Session::open(&core), next: 0 };
    let folder = temp_folder("core-host-shell-cwd");

    let ran = server.run(serde_json::json!({ "command": "echo out; echo err >&2; pwd; exit 3", "cwd": folder }));
    assert_eq!(ran, serde_json::json!({ "exit_code": 3, "stdout": format!("out\n{}\n", folder.display()), "stderr": "err\n", "timed_out": false, "cwd": folder }));
    assert_eq!(server.run(serde_json::json!({ "command": "pwd" }))["stdout"], format!("{}\n", home.display()), "home by default");
    let env = server.run(serde_json::json!({ "command": "echo \"$JULIA_DEPOT_PATH|$ENDEAVOR_TOKEN|$ENDEAVOR_LAUNCHER\"" }));
    assert_eq!(env["stdout"], "/opt/depot:||\n", "Julia's environment, without the runtime's secrets");

    // A session's own folder is where it runs by default; Julia keeps it too.
    let set = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"endeavor/set_session_folder","params":{{"owner":"8","folder":"{}"}}}}"#, folder.display());
    assert!(app_call(&core, &set).contains("set_session_folder"), "Julia's reply");
    assert_eq!(server.call("8", "run_shell", serde_json::json!({ "command": "pwd" })).unwrap()["stdout"], format!("{}\n", folder.display()));
    assert_eq!(server.call("8", "run_shell", serde_json::json!({ "command": "pwd", "cwd": "~" })).unwrap()["stdout"], format!("{}\n", home.display()));
    app_call(&core, &set.replace(&folder.display().to_string(), ""));
    assert_eq!(server.call("8", "run_shell", serde_json::json!({ "command": "pwd" })).unwrap()["stdout"], format!("{}\n", home.display()));

    let error = |kind: &str, message: &str| Err(serde_json::json!({ "error": kind, "message": message }));
    assert_eq!(server.call("", "run_shell", serde_json::json!({ "command": "  " })), error("invalid_argument", "command is empty"));
    assert_eq!(server.call("", "run_shell", serde_json::json!({})), error("invalid_argument", "command must be a string"));
    assert_eq!(server.call("", "run_shell", serde_json::json!({ "command": "true", "cwd": "nope" })), error("not_found", &format!("No folder at {}/nope", home.display())));
    assert_eq!(server.call("", "run_shell", serde_json::json!({ "command": "true", "timeout_seconds": "x" })), error("invalid_argument", "timeout_seconds must be a whole number"));

    let loud = server.run(serde_json::json!({ "command": "yes | head -c 100000" }));
    let stdout = loud["stdout"].as_str().unwrap();
    assert!(stdout.contains("\n[… 70000 bytes left out …]\n") && stdout.len() < 30_100, "{}", stdout.len());

    let killed = server.run(serde_json::json!({ "command": "kill -9 $$" }));
    assert_eq!((&killed["exit_code"], &killed["timed_out"]), (&serde_json::Value::Null, &serde_json::json!(false)));

    // A timeout kills everything the command started, background jobs included.
    let started = std::time::Instant::now();
    let slow = server.run(serde_json::json!({ "command": "sleep 5 & sleep 5", "timeout_seconds": 1 }));
    assert_eq!((&slow["exit_code"], &slow["timed_out"]), (&serde_json::Value::Null, &serde_json::json!(true)));
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());

    // A background job that outlives the command holds its output open only so long.
    let started = std::time::Instant::now();
    let lingering = server.run(serde_json::json!({ "command": "echo done; sleep 30 &" }));
    assert_eq!((&lingering["exit_code"], &lingering["stdout"]), (&serde_json::json!(0), &serde_json::json!("done\n")));
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn refuses_browsers_foreign_hosts_and_callers_without_the_token() {
    let dir = state_dir("core-refuse");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let ask = |request: &str| {
        let mut socket = core.connect();
        socket.write_all(request.as_bytes()).unwrap();
        let (status, _, body) = response(&mut BufReader::new(socket));
        (status, body)
    };
    let auth = format!("Authorization: Bearer {TOKEN}\r\n");
    let refused = |status: &str, error: &str| (format!("HTTP/1.1 {status}"), format!(r#"{{"error":"{error}"}}"#));
    let seen_before = bridge.seen().len();
    for route in ["GET /sse", "POST /message?sessionId=x", "POST /call", "GET /events", "POST /dispatch", "GET /nope", "GET /health"] {
        let request = |headers: &str| format!("{route} HTTP/1.1\r\n{headers}Content-Length: 2\r\n\r\n{{}}");
        if route != "GET /health" {
            assert_eq!(ask(&request("Host: 127.0.0.1\r\n")), refused("401 Unauthorized", "unauthorized"), "{route}");
            assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\n{}", auth.replace('0', "1")))), refused("401 Unauthorized", "unauthorized"));
            assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\n{}", auth.replace("\r\n", "0\r\n")))), refused("401 Unauthorized", "unauthorized"));
        }
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\nOrigin: https://example.com\r\n{auth}"))), refused("403 Forbidden", "browser_origin_refused"));
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\nOrigin: null\r\n{auth}"))), refused("403 Forbidden", "browser_origin_refused"));
        assert_eq!(ask(&request(&format!("Host: evil.example:80\r\n{auth}"))), refused("403 Forbidden", "host_not_loopback"), "{route}");
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1.evil.example\r\n{auth}"))), refused("403 Forbidden", "host_not_loopback"));
        assert_eq!(ask(&request(&auth)), refused("403 Forbidden", "host_not_loopback"));
    }
    assert_eq!(bridge.seen().len(), seen_before, "nothing refused reaches Julia");
    for host in ["localhost", "[::1]:9", "127.0.0.1:9"] {
        let request = format!("POST /message?sessionId=x HTTP/1.1\r\nHost: {host}\r\n{auth}Content-Length: 2\r\n\r\n{{}}");
        assert_eq!(ask(&request).0, "HTTP/1.1 404 Not Found", "{host} is loopback");
    }
    assert_eq!(ask(&format!("GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")), ("HTTP/1.1 200 OK".into(), "ok".into()), "no token needed");

    // A refused request leaves the connection for the next one.
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    write!(socket, "POST /call HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\n{{}}").unwrap();
    assert_eq!(response(&mut reader).0, "HTTP/1.1 401 Unauthorized");
    write!(socket, "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    assert_eq!(response(&mut reader).2, "ok");
}

#[test]
fn exits_when_julia_does_and_cleans_up() {
    let dir = state_dir("core-exit");
    let bridge = FakeBridge::start(&dir);
    let mut core = Core::start(&dir, &bridge);
    // SAFETY: plain syscall.
    unsafe { libc::kill(core.julia_pid, libc::SIGKILL) };
    assert_eq!(core.exits().signal(), Some(libc::SIGKILL), "the same way Julia did");
    assert!(!dir.join("runtime.json").exists() && !dir.join("julia.json").exists());
}

#[test]
fn a_stop_signal_to_the_core_stops_julia() {
    let dir = state_dir("core-stop");
    let bridge = FakeBridge::start(&dir);
    let mut core = Core::start(&dir, &bridge);
    // SAFETY: plain syscall.
    unsafe { libc::kill(core.process.id() as i32, libc::SIGTERM) };
    assert_eq!(core.exits().signal(), Some(libc::SIGTERM));
    wait_for("Julia to exit", || !pid_alive(core.julia_pid));
    assert!(!dir.join("runtime.json").exists());
}
