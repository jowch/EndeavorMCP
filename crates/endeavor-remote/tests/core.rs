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
        "POST /call HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nX-Endeavor-Host: labbox3\r\nOrigin: https://example.com\r\nContent-Length: {}\r\n\r\n{body}",
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
    assert_eq!(seen.header("Origin"), Some("https://example.com"), "Julia still decides about browsers");

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

#[test]
fn carries_mcp_over_sse_and_message() {
    let dir = state_dir("core-mcp");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut stream = core.connect();
    let mut events = BufReader::new(stream.try_clone().unwrap());
    write!(stream, "GET /sse HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nAccept: text/event-stream\r\n\r\n").unwrap();
    read_until(&mut events, "data: /message?sessionId=s1");

    let message = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let mut post = core.connect();
    write!(
        post,
        "POST /message?sessionId=s1 HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{message}",
        message.len()
    )
    .unwrap();
    assert_eq!(response(&mut BufReader::new(post)).0, "HTTP/1.1 202 Accepted");
    read_until(&mut events, &format!("event: message\ndata: {message}"));
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
