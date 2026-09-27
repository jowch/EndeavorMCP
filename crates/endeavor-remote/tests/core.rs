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
        let julia = serving_julia(dir, bridge);
        let process = Command::new(env!("CARGO_BIN_EXE_endeavor-remote"))
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
    let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_notebooks"}}"#;
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

    let message = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    assert_eq!(post(&core, &session.id, message, &caller), ("HTTP/1.1 202 Accepted".into(), String::new()));
    assert_eq!(session.reply(), r#"{"id":1,"jsonrpc":"2.0","result":{"host":"gpu-box","method":"tools/list","owner":"7"}}"#);
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
