//! `endeavor core` with a stand-in Julia (see `common`): it starts it,
//! writes `runtime.json` once it's ready, passes the requests on its port it
//! doesn't answer through to Pluto or Julia's bridge, streams as they're
//! written, lets a browser in with its cookie, and lives and dies with it.

#![cfg(unix)]

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
        Core::start_with_env(dir, bridge, &[("HOME", home)])
    }

    fn start_with_env(dir: &Path, bridge: &FakeBridge, env: &[(&str, &str)]) -> Core {
        let julia = serving_julia(dir, bridge);
        let process = Command::new(env!("CARGO_BIN_EXE_endeavor"))
            .envs(env.iter().copied())
            .arg("core")
            .arg("--state-dir")
            .arg(dir)
            .arg("--julia")
            .arg(&julia)
            .args(["--runtime", "/opt/runtime", "--depot", "/opt/depot:"])
            .env("ENDEAVOR_TOKEN", TOKEN)
            .env("ENDEAVOR_LAUNCHER", "process")
            .env("ENDEAVOR_BUILD", "1.0.0-abc")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        wait_for("runtime.json", || dir.join("runtime.json").exists());
        let state = read_json(&dir.join("runtime.json"));
        let julia_pid = read_json(&dir.join("julia.json"))["pid"].as_i64().unwrap() as i32;
        Core { port: state["port"].as_u64().unwrap() as u16, process, julia_pid }
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
    assert!(core.port != bridge.port && core.port != bridge.pluto_port, "the core's own port");
    let mut keys: Vec<&str> = state.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["build", "exits_when_idle", "job", "launcher", "node", "pid", "port", "started", "token"], "nothing of Pluto's");
    assert_eq!(state["exits_when_idle"], false, "started without ENDEAVOR_EXIT_IDLE");
    assert_eq!((state["token"].as_str(), state["launcher"].as_str()), (Some(TOKEN), Some("process")));
    assert_eq!(state["build"].as_str(), Some("1.0.0-abc"), "the build it was started from");
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
    let body = r#"{"jsonrpc":"2.0","id":7,"method":"endeavor/set_folder","params":{"path":"/n"}}"#;
    write!(
        socket,
        "POST /endeavor/call HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nX-Endeavor-Host: labbox3\r\nContent-Length: {}\r\n\r\n{body}",
        core.port,
        body.len()
    )
    .unwrap();
    let (status, headers, reply) = response(&mut reader);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains(&("transfer-encoding".into(), "chunked".into())), "Julia's framing passes through");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["result"]["body"].as_str(), Some(body));

    let seen = bridge.seen().into_iter().find(|s| s.line == "POST /call HTTP/1.1").expect("Julia's `/call`");
    assert_eq!(seen.header("Host"), Some(format!("127.0.0.1:{}", bridge.port).as_str()));
    assert_eq!(seen.header("Authorization"), Some(format!("Bearer {TOKEN}").as_str()));
    assert_eq!(seen.header("Content-Type"), Some("application/json"));
    assert_eq!(seen.header("X-Endeavor-Host"), Some("labbox3"));

    // A chunked call, which the core reads whole to see its method, reaches Julia whole.
    write!(socket, "POST /endeavor/call HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
    write!(socket, "{:x}\r\n{}\r\n", 20, &body[..20]).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    write!(socket, "{:x}\r\n{}\r\n0\r\n\r\n", body.len() - 20, &body[20..]).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&response(&mut reader).2).unwrap();
    assert_eq!(reply["result"]["body"].as_str(), Some(body));

    // The same connection carries the next request.
    write!(socket, "GET /endeavor/nope HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    assert_eq!(response(&mut reader).0, "HTTP/1.1 404 Not Found");
    write!(socket, "GET /edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    assert_eq!(response(&mut reader).2, "Pluto: GET /edit?id=1 HTTP/1.1");

    // HTTP/1.0, as the helper's own calls are: the reply runs to the end of the connection.
    let mut socket = core.connect();
    write!(socket, "POST /endeavor/call HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
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
    let seen = bridge.pluto_seen().pop().unwrap();
    assert_eq!((seen.header("Cookie"), seen.header("Authorization")), (Some("secret=s3cret"), None), "Pluto gets its secret, not the token");

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
    write!(socket, "GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
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
    write!(socket, "GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    bridge.event("third");
    read_until(&mut reader, "data: third");
    bridge.drop_events();
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
}

/// A notebook as the adapter's `snapshot` reports it, with one cell.
fn notebook(id: &str, code: &str) -> serde_json::Value {
    serde_json::json!({
        "notebook_id": id, "path": "/n/a.jl", "cell_order": ["c1"], "execution_allowed": true, "safe_preview": false, "pending_run": [],
        "cells": [{ "cell_id": "c1", "code": code, "running": false, "queued": false, "errored": false }],
    })
}

#[test]
fn serves_the_apps_events_from_what_the_adapter_reports() {
    let dir = state_dir("core-own-events");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    wait_for("the core to follow Julia's notifications", || bridge.seen().iter().any(|s| s.line == "GET /notifications HTTP/1.0"));
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    // HTTP/1.0, as the app asks: the stream runs to the end of the connection.
    write!(socket, "GET /endeavor/events HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    let head = read_until(&mut reader, "\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n") && head.contains("Content-Type: text/event-stream\r\n") && !head.contains("chunked"), "{head}");
    assert_eq!(read_until(&mut reader, "\n\n"), "data: {\"asks\":[],\"build\":\"1.0.0-abc\",\"cells\":{},\"idle_stopped\":[],\"notebooks\":[]}\n\n", "the state now, and the build it came from");

    // Julia says a notebook changed: the core reads it and tells the app.
    bridge.set_notebooks(vec![notebook("n1", "x = 1")]);
    bridge.notify(serde_json::json!({ "method": "notebook_opened", "params": { "notebook_id": "n1", "path": "/n/a.jl" } }));
    let event = read_until(&mut reader, "\n\n");
    let event: serde_json::Value = serde_json::from_str(event.strip_prefix("data: ").unwrap().trim_end()).unwrap();
    assert_eq!(
        event["notebooks"],
        serde_json::json!([{ "notebook_id": "n1", "path": "/n/a.jl", "cell_count": 1, "pending_run": [], "running": [], "execution_allowed": true, "this_session": false }])
    );
    assert_eq!(event["cells"]["n1"][0]["author"], serde_json::Value::Null);

    // The same state again isn't news; a change the tools didn't make is the user's.
    bridge.notify(serde_json::json!({ "method": "topology_changed", "params": { "notebook_id": "n1" } }));
    bridge.set_notebooks(vec![notebook("n1", "x = 2")]);
    bridge.notify(serde_json::json!({ "method": "cell_state", "params": { "notebook_id": "n1", "cells": [{ "cell_id": "c1", "code": "x = 2" }] } }));
    let event = read_until(&mut reader, "\n\n");
    let event: serde_json::Value = serde_json::from_str(event.strip_prefix("data: ").unwrap().trim_end()).unwrap();
    assert_eq!(event["cells"]["n1"][0]["author"], "user");

    // Tool calls the core answers are news too, and don't reach Julia.
    let keep = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"keep_notebook_alive","arguments":{"notebook_id":"aaaaaaaa-0000-0000-0000-000000000000","keep":true}}}"#;
    let (status, body) = mcp(&core, keep, &[("X-Endeavor-Session", "7")]);
    assert_eq!(status, "HTTP/1.1 200 OK");
    let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
    let error: serde_json::Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(error["error"], "notebook_not_found");
    assert_eq!(
        error["message"],
        "No notebook with id 'aaaaaaaa-0000-0000-0000-000000000000' in the current session. Run list_notebooks to see what's open.\n\
         See `notebook_guide` for how to use these tools."
    );
    assert!(!bridge.seen().iter().any(|s| s.line.starts_with("POST /dispatch")));
}

/// POST one JSON-RPC message to `/mcp`, as the agent's MCP client does: the
/// response's status and body (a request's reply, or nothing for `202`).
fn mcp(core: &Core, message: &str, caller: &[(&str, &str)]) -> (String, String) {
    let (status, _, body) = mcp_response(core, message, caller);
    (status, body)
}

/// `mcp`, with the response's headers (names in lower case).
fn mcp_response(core: &Core, message: &str, caller: &[(&str, &str)]) -> (String, Vec<(String, String)>, String) {
    let mut socket = core.connect();
    let headers: String = caller.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
    write!(
        socket,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\n\r\n{message}",
        core.port,
        message.len()
    )
    .unwrap();
    response(&mut BufReader::new(socket))
}

#[test]
fn an_agent_without_the_session_header_is_told_apart_by_its_mcp_session_id() {
    let dir = state_dir("core-mcp-session");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let folder = temp_folder("core-mcp-session-notebooks");
    let (a, b) = (folder.join("a.jl").display().to_string(), folder.join("b.jl").display().to_string());
    for path in [&a, &b] {
        std::fs::write(path, "### A Pluto.jl notebook ###").unwrap();
    }
    let at = |id: &str, path: &str| {
        let mut nb = notebook(id, "x = 1");
        nb["path"] = path.into();
        nb
    };
    bridge.set_notebooks(vec![at("aaaaaaaa-0000-0000-0000-000000000001", &a), at("aaaaaaaa-0000-0000-0000-000000000002", &b)]);

    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#;
    let session_of = |caller: &[(&str, &str)]| {
        let (status, headers, _) = mcp_response(&core, initialize, caller);
        assert_eq!(status, "HTTP/1.1 200 OK");
        headers.into_iter().find(|(name, _)| name == "mcp-session-id").map(|(_, id)| id)
    };
    let first = session_of(&[]).expect("a client without X-Endeavor-Session gets a session id");
    let second = session_of(&[]).unwrap();
    assert!(first.starts_with("mcp-") && first.len() == 36 && first[4..].chars().all(|c| c.is_ascii_hexdigit()), "{first}");
    assert_ne!(first, second, "each initialize starts its own session");
    assert_eq!(session_of(&[("X-Endeavor-Session", "7")]), None, "the app's sessions have a key already");
    assert_eq!(session_of(&[("Mcp-Session-Id", &first)]), None, "nor does a client that has one");
    let (_, headers, _) = mcp_response(&core, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &[]);
    assert!(!headers.iter().any(|(name, _)| name == "mcp-session-id"), "only initialize issues one: {headers:?}");

    let call = |caller: &[(&str, &str)], name: &str, arguments: serde_json::Value| {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        let reply: serde_json::Value = serde_json::from_str(&mcp(&core, &message.to_string(), caller).1).unwrap();
        serde_json::from_str::<serde_json::Value>(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let marked = |caller: &[(&str, &str)]| -> Vec<(String, bool)> {
        let listed = call(caller, "list_notebooks", serde_json::json!({}));
        listed.as_array().unwrap().iter().map(|nb| (nb["path"].as_str().unwrap().to_owned(), nb["this_session"] == true)).collect()
    };
    let one = [("Mcp-Session-Id", first.as_str())];
    let two = [("Mcp-Session-Id", second.as_str())];

    assert_eq!(marked(&one), vec![(a.clone(), false), (b.clone(), false)], "notebooks it didn't open are someone else's");
    assert_eq!(call(&one, "open_notebook", serde_json::json!({ "path": a }))["path"], serde_json::json!(a));
    assert_eq!(marked(&one), vec![(a.clone(), true), (b.clone(), false)], "the notebook it opened is this session's");
    assert_eq!(marked(&two), vec![(a.clone(), false), (b.clone(), false)], "another session's isn't");
    assert_eq!(marked(&[]), vec![(a.clone(), false), (b.clone(), false)], "nor a client's with no session at all");
    assert_eq!(marked(&[("X-Endeavor-Session", "7")]), vec![(a.clone(), false), (b.clone(), false)], "nor an app session's");

    let refused = call(&one, "open_notebook", serde_json::json!({ "path": b }));
    assert_eq!(refused["error"], "one_notebook");
    assert_eq!(
        refused["message"],
        format!(
            "This session works on one notebook, {a}, so it can't open {b}. You can still read other notebooks as plain .jl files. \
             To work on another notebook, suggest the user start a new session with it.\n\
             See `notebook_guide` for how to use these tools."
        )
    );
    assert_eq!(call(&two, "open_notebook", serde_json::json!({ "path": b }))["path"], serde_json::json!(b), "the other session may");
    assert_eq!(marked(&two), vec![(a.clone(), false), (b.clone(), true)]);
}

#[test]
fn sessions_join_an_open_notebook_and_the_runtime_says_how_many_called_lately() {
    let dir = state_dir("core-join");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let folder = temp_folder("core-join-notebooks");
    let path = folder.join("a.jl").display().to_string();
    std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
    let id = "aaaaaaaa-0000-0000-0000-000000000001";
    let mut open = notebook(id, "x = 1");
    open["path"] = path.as_str().into();
    bridge.set_notebooks(vec![open]);

    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"codex-cli","version":"1"}}}"#;
    let (_, headers, _) = mcp_response(&core, initialize, &[]);
    let plain = headers.into_iter().find(|(name, _)| name == "mcp-session-id").unwrap().1;
    let call = |caller: &[(&str, &str)], name: &str, arguments: serde_json::Value| {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        let reply: serde_json::Value = serde_json::from_str(&mcp(&core, &message.to_string(), caller).1).unwrap();
        serde_json::from_str::<serde_json::Value>(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let recent = |owner: &str| -> serde_json::Value {
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "endeavor/recent_sessions", "params": { "owner": owner, "within_seconds": 900 } });
        serde_json::from_str::<serde_json::Value>(&app_call(&core, &body.to_string())).unwrap()["result"].clone()
    };

    // The first session opens the notebook; the second joins it.
    let one = [("Mcp-Session-Id", plain.as_str())];
    let opened = call(&one, "open_notebook", serde_json::json!({ "path": path }));
    assert_eq!((&opened["notebook_id"], opened.get("already_open")), (&serde_json::json!(id), Some(&serde_json::json!(true))), "the notebook was open already");
    let two = [("X-Endeavor-Session", "stdio-1")];
    let joined = call(&two, "open_notebook", serde_json::json!({ "path": path, "run_notebook": true }));
    assert_eq!((&joined["notebook_id"], &joined["already_open"], &joined["ran"]), (&serde_json::json!(id), &serde_json::json!(true), &serde_json::json!(false)));
    let marked = call(&two, "list_notebooks", serde_json::json!({}));
    assert_eq!(marked[0]["this_session"], true);

    // Each is another to the other; the app, which has no session, finds both; neither leaves a record for the check itself.
    for (owner, count) in [("stdio-1", 1), (plain.as_str(), 1), ("", 2), ("nobody", 2)] {
        let said = recent(owner);
        assert!((said["count"].as_u64(), said["active_seconds_ago"].as_u64().map(|s| s < 5)) == (Some(count), Some(true)), "{owner:?}: {said}");
    }
    let refused = app_call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"endeavor/recent_sessions","params":{"owner":"a"}}"#);
    assert!(refused.contains("within_seconds must be a number"), "{refused}");
}

#[test]
fn a_call_with_arguments_the_tool_does_not_take_is_refused_and_is_not_the_sessions_activity() {
    let dir = state_dir("core-arguments");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let folder = temp_folder("core-arguments-notebooks");
    let path = folder.join("a.jl").display().to_string();
    std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
    let mut open = notebook("aaaaaaaa-0000-0000-0000-000000000001", "x = 1");
    open["path"] = path.as_str().into();
    bridge.set_notebooks(vec![open]);

    let caller = [("X-Endeavor-Session", "s1")];
    let send = |id: u32, name: &str, arguments: serde_json::Value| {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        mcp(&core, &message.to_string(), &caller).1
    };
    let guide = "\nSee `notebook_guide` for how to use these tools.";
    assert_eq!(
        send(1, "new_notebook", serde_json::json!({ "name": "remote.jl" })),
        tool_error(1, "invalid_argument", &format!("`name` is not an argument of `new_notebook`. Its arguments: `path`.{guide}"))
    );
    assert!(!send(2, "list_notebooks", serde_json::json!({ "input": "" })).contains("invalid_argument"), "a tool with no arguments ignores what it is given");
    assert!(send(10, "add_cell", serde_json::json!({ "notebook_id": "n1" })).contains("invalid_notebook_id"), "add_cell without code passes the check");
    assert_eq!(
        send(3, "edit_cell", serde_json::json!({ "code": "1" })),
        tool_error(3, "invalid_argument", &format!("`edit_cell` needs `notebook_id`, `cell_id`.{guide}"))
    );
    assert_eq!(
        send(4, "read_notebook_code", serde_json::json!({})),
        tool_error(4, "invalid_argument", &format!("`read_notebook_code` needs `notebook_id`.{guide}"))
    );
    assert!(!send(5, "list_notebooks", serde_json::json!({})).contains("invalid_argument"), "a valid call still runs");

    // Calls that were refused leave no mark on the session: its last call stays the one that opened the notebook.
    let opened = serde_json::json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "open_notebook", "arguments": { "path": path } } });
    mcp(&core, &opened.to_string(), &caller);
    std::thread::sleep(Duration::from_millis(2100));
    let ago = || {
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "endeavor/recent_sessions", "params": { "owner": "", "within_seconds": 900 } });
        serde_json::from_str::<serde_json::Value>(&app_call(&core, &body.to_string())).unwrap()["result"]["active_seconds_ago"].as_u64().unwrap()
    };
    send(7, "new_notebook", serde_json::json!({ "name": "remote.jl" }));
    send(8, "edit_cell", serde_json::json!({}));
    assert!(ago() >= 2, "a refused call counted as activity");
    send(9, "list_notebooks", serde_json::json!({}));
    assert!(ago() < 2);
}

#[test]
fn serves_the_agents_mcp_messages() {
    let dir = state_dir("core-mcp");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let caller = [("X-Endeavor-Session", "7"), ("X-Endeavor-Host", "gpu-box")];

    // A request gets its reply in the same response.
    let message = r#"{"jsonrpc":"2.0","id":1,"method":"resources/list","params":{}}"#;
    assert_eq!(mcp(&core, message, &caller), ("HTTP/1.1 200 OK".into(), r#"{"error":{"code":-32601,"message":"Method not found: resources/list"},"id":1,"jsonrpc":"2.0"}"#.into()));
    assert_eq!(mcp(&core, r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#, &caller), ("HTTP/1.1 200 OK".into(), r#"{"id":"p","jsonrpc":"2.0","result":{}}"#.into()));
    // A notification gets 202 and no body.
    assert_eq!(mcp(&core, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &caller), ("HTTP/1.1 202 Accepted".into(), String::new()));
    let init = mcp(&core, r#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#, &[("X-Endeavor-Skills", "plugin")]);
    assert_eq!(
        init,
        (
            "HTTP/1.1 200 OK".into(),
            format!(r#"{{"id":2,"jsonrpc":"2.0","result":{{"capabilities":{{"tools":{{}}}},"protocolVersion":"2025-06-18","serverInfo":{{"name":"endeavor-runtime","version":"{}"}}}}}}"#, env!("CARGO_PKG_VERSION"))
        )
    );
    // An agent without Endeavor's plugin is told to read the guide, and can.
    let init: serde_json::Value = serde_json::from_str(&mcp(&core, r#"{"jsonrpc":"2.0","id":3,"method":"initialize"}"#, &[]).1).unwrap();
    assert!(init["result"]["instructions"].as_str().unwrap().contains("call `notebook_guide` once"));
    let guide = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"notebook_guide","arguments":{}}}"#;
    let guide: serde_json::Value = serde_json::from_str(&mcp(&core, guide, &caller).1).unwrap();
    assert_eq!(guide["result"]["isError"], false);
    assert!(guide["result"]["content"][0]["text"].as_str().unwrap().contains("# Working in a live notebook"));
    assert!(!bridge.seen().iter().any(|s| s.line.starts_with("POST /dispatch")));

    assert_eq!(mcp(&core, "{nope", &caller), ("HTTP/1.1 400 Bad Request".into(), r#"{"error":"Invalid JSON"}"#.into()));
    assert_eq!(mcp(&core, "[1]", &caller).0, "HTTP/1.1 400 Bad Request");
}

#[test]
fn honors_the_mcp_protocol_version_header() {
    let dir = state_dir("core-mcp-version");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for version in ["2025-06-18", "2025-03-26", "2024-11-05"] {
        assert_eq!(mcp(&core, ping, &[("MCP-Protocol-Version", version)]), ("HTTP/1.1 200 OK".into(), r#"{"id":1,"jsonrpc":"2.0","result":{}}"#.into()), "{version}");
    }
    assert_eq!(mcp(&core, ping, &[]).0, "HTTP/1.1 200 OK", "no header falls back to the spec's default");
    assert_eq!(mcp(&core, ping, &[("MCP-Protocol-Version", "2099-01-01")]).0, "HTTP/1.1 400 Bad Request");
}

#[test]
fn negotiates_the_protocol_version_on_initialize() {
    let dir = state_dir("core-mcp-negotiate");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let version = |body: &str| {
        let (status, reply) = mcp(&core, body, &[]);
        assert_eq!(status, "HTTP/1.1 200 OK");
        let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
        reply["result"]["protocolVersion"].as_str().unwrap().to_owned()
    };
    // No protocolVersion in params: our latest.
    assert_eq!(version(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#), "2025-06-18");
    // A supported version, not the latest: echoed back.
    assert_eq!(version(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#), "2024-11-05");
    assert_eq!(version(r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#), "2025-03-26");
    // A version we don't speak: our latest, per the spec's fallback (initialize never errors on this).
    assert_eq!(version(r#"{"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#), "2025-06-18");
}

#[test]
fn only_post_is_allowed_on_mcp() {
    let dir = state_dir("core-mcp-methods");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let ask = |request: &str| {
        let mut socket = core.connect();
        socket.write_all(request.as_bytes()).unwrap();
        response(&mut BufReader::new(socket)).0
    };
    assert_eq!(ask(&format!("GET /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n")), "HTTP/1.1 405 Method Not Allowed");
    assert_eq!(ask(&format!("DELETE /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n")), "HTTP/1.1 405 Method Not Allowed");
}

#[test]
fn keeps_concurrent_sessions_apart() {
    let dir = state_dir("core-sessions");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    std::thread::scope(|scope| {
        for n in 0..2 {
            for i in 0..10 {
                let core = &core;
                scope.spawn(move || {
                    let message = format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"tools/call","params":{{"name":"tool_{n}","arguments":{{}}}}}}"#);
                    let (status, body) = mcp(core, &message, &[("X-Endeavor-Session", &n.to_string())]);
                    assert_eq!(status, "HTTP/1.1 200 OK");
                    let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(reply["id"], i);
                    assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains(&format!("tool_{n}")));
                });
            }
        }
    });
}

/// One of the app's `/call`s, as the app makes them: the reply's body.
fn app_call(core: &Core, body: &str) -> String {
    let mut socket = core.connect();
    write!(socket, "POST /endeavor/call HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", core.port, body.len()).unwrap();
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
fn the_apps_notebook_calls_are_the_cores() {
    let dir = state_dir("core-app-calls");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let file = dir.join("a.jl");
    std::fs::write(&file, "").unwrap();
    let info = app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"endeavor/file_info","params":{{"path":"{}"}}}}"#, file.display()));
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!((&info["id"], &info["result"]["exists"]), (&serde_json::json!(1), &serde_json::json!(true)));
    assert!(info["result"]["modified"].as_f64().unwrap() > 1.7e9);
    let missing = r#"{"code":-32000,"message":"KeyError: key \"notebook_not_found::No notebook with id 'n9' in the current session\" not found"}"#;
    let restart = r#"{"jsonrpc":"2.0","id":2,"method":"endeavor/restart_notebook","params":{"notebook_id":"n9"}}"#;
    assert_eq!(app_call(&core, restart), format!(r#"{{"error":{missing},"id":2,"jsonrpc":"2.0"}}"#));
    let moved = r#"{"jsonrpc":"2.0","id":3,"method":"endeavor/move_notebook","params":{"notebook_id":"n9","path":"/tmp/b.jl"}}"#;
    assert_eq!(app_call(&core, moved), format!(r#"{{"error":{missing},"id":3,"jsonrpc":"2.0"}}"#));
    let to_julia: Vec<String> = bridge.seen().iter().filter(|s| s.line.starts_with("POST /call")).map(|s| String::from_utf8_lossy(&s.body).into_owned()).collect();
    assert!(!to_julia.iter().any(|body| body.contains("_notebook") || body.contains("file_info")), "none reached Julia's /call: {to_julia:?}");
}

#[test]
fn the_app_looks_up_a_sessions_tool_results() {
    let dir = state_dir("core-results");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let keep = |id: u32, meta: &str| {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"keep_notebook_alive","arguments":{{"keep":true,"notebook_id":"aaaaaaaa-0000-0000-0000-000000000000"}}{meta}}}}}"#)
    };
    let (_, said) = mcp(&core, &keep(1, r#","_meta":{"claudecode/toolUseId":"toolu_01"}"#), &[("X-Endeavor-Session", "7")]);
    mcp(&core, &keep(2, ""), &[("X-Endeavor-Session", "7")]);
    let said: serde_json::Value = serde_json::from_str(&said).unwrap();

    let look_up = |params: &str| app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":9,"method":"endeavor/tool_result","params":{params}}}"#));
    let found: serde_json::Value = serde_json::from_str(&look_up(r#"{"owner":"7","call_id":"toolu_01","tool":"keep_notebook_alive","arguments":{}}"#)).unwrap();
    assert_eq!(found["result"], serde_json::json!({ "content": said["result"]["content"], "isError": true }), "by the id Claude Code sends");
    // An agent whose call ids the runtime never saw: by tool and arguments, oldest first, each once.
    let by_arguments = r#"{"owner":"7","call_id":"cursor-1","tool":"keep_notebook_alive","arguments":{"notebook_id":"aaaaaaaa-0000-0000-0000-000000000000","keep":true}}"#;
    let found: serde_json::Value = serde_json::from_str(&look_up(by_arguments)).unwrap();
    assert_eq!(found["result"]["isError"], true);
    assert_eq!(look_up(by_arguments), r#"{"id":9,"jsonrpc":"2.0","result":null}"#, "both calls were looked up");
    assert_eq!(look_up(r#"{"owner":"8","tool":"keep_notebook_alive","arguments":{}}"#), r#"{"id":9,"jsonrpc":"2.0","result":null}"#);
}

/// Follow the app's `/endeavor/events` stream until `done` holds for an event; that event.
fn event_where(reader: &mut impl BufRead, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    loop {
        let event = read_until(reader, "\n\n");
        let event: serde_json::Value = serde_json::from_str(event.trim_start_matches(|c| c != 'd').strip_prefix("data: ").unwrap().trim_end()).unwrap();
        if done(&event) {
            return event;
        }
    }
}

#[test]
fn in_ask_to_run_a_run_waits_for_the_users_answer() {
    let dir = state_dir("core-asks");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let caller = [("X-Endeavor-Session", "7"), ("X-Endeavor-Host", "gpu-box")];
    let shell = |id: u32| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"run_shell","arguments":{{"command":"echo ran"}},"_meta":{{"claudecode/toolUseId":"toolu_{id}"}}}}}}"#);
    let text = |body: &str| {
        let reply: serde_json::Value = serde_json::from_str(body).unwrap();
        let text: serde_json::Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        text
    };
    let policy = |policy: &str, asks: bool| app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"endeavor/set_policy","params":{{"owner":"7","policy":"{policy}","asks":{asks}}}}}"#));

    // An app from before runtime asks never turns them on: the run goes ahead.
    policy("ask", false);
    assert_eq!(text(&mcp(&core, &shell(1), &caller).1)["stdout"], "ran\n");
    // On, with no app following to ask: it fails at once.
    policy("ask", true);
    assert_eq!(text(&mcp(&core, &shell(2), &caller).1)["error"], "no_app");
    let open = |run: bool| format!(r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"open_notebook","arguments":{{"path":"/nope.jl","run_notebook":{run}}}}}}}"#);
    assert_eq!(text(&mcp(&core, &open(true), &caller).1)["error"], "no_app", "opening to run asks like a run");
    assert_ne!(text(&mcp(&core, &open(false), &caller).1)["error"], "no_app", "opening without running doesn't");

    let mut events = core.connect();
    let mut reader = BufReader::new(events.try_clone().unwrap());
    write!(events, "GET /endeavor/events HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    event_where(&mut reader, |_| true);
    let answer = |id: &serde_json::Value, allow: bool| app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":2,"method":"endeavor/answer_run","params":{{"id":{id},"allow":{allow},"user_ran":[]}}}}"#));
    std::thread::scope(|scope| {
        // Allowed: it runs once the user says so.
        let call = scope.spawn(|| mcp(&core, &shell(3), &caller).1);
        let ask = event_where(&mut reader, |e| e["asks"].as_array().is_some_and(|a| !a.is_empty()))["asks"][0].clone();
        assert_eq!((&ask["owner"], &ask["call_id"], &ask["tool"], &ask["arguments"]), (&"7".into(), &"toolu_3".into(), &"run_shell".into(), &serde_json::json!({ "command": "echo ran" })));
        assert!(!call.is_finished(), "waits");
        assert_eq!(answer(&ask["id"], true), r#"{"id":2,"jsonrpc":"2.0","result":{}}"#);
        assert_eq!(text(&call.join().unwrap())["stdout"], "ran\n");
        event_where(&mut reader, |e| e["asks"] == serde_json::json!([]));
        assert!(answer(&ask["id"], true).contains("no_ask"), "answered once");

        // Denied: it doesn't run, and the agent hears why.
        let call = scope.spawn(|| mcp(&core, &shell(4), &caller).1);
        let ask = event_where(&mut reader, |e| e["asks"].as_array().is_some_and(|a| !a.is_empty()))["asks"][0].clone();
        answer(&ask["id"], false);
        assert_eq!(text(&call.join().unwrap()), serde_json::json!({ "error": "not_approved", "message": "The user chose not to run this." }));

        // Cancelled by the agent: the ask goes.
        let call = scope.spawn(|| mcp(&core, &shell(5), &caller).1);
        event_where(&mut reader, |e| e["asks"].as_array().is_some_and(|a| !a.is_empty()));
        assert_eq!(mcp(&core, r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}"#, &caller).0, "HTTP/1.1 202 Accepted");
        assert_eq!(text(&call.join().unwrap())["error"], "cancelled");
        event_where(&mut reader, |e| e["asks"] == serde_json::json!([]));

        // The agent hangs up: the ask goes.
        let mut socket = core.connect();
        let message = shell(6);
        write!(socket, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nX-Endeavor-Session: 7\r\nX-Endeavor-Host: gpu-box\r\nContent-Length: {}\r\n\r\n{message}", message.len()).unwrap();
        event_where(&mut reader, |e| e["asks"].as_array().is_some_and(|a| !a.is_empty()));
        drop(socket);
        event_where(&mut reader, |e| e["asks"] == serde_json::json!([]));
    });

    // Auto runs without asking; so does a call that runs nothing.
    policy("auto", true);
    assert_eq!(text(&mcp(&core, &shell(7), &caller).1)["stdout"], "ran\n");
}

#[test]
fn a_held_call_answers_as_an_event_stream_at_once() {
    let dir = state_dir("core-held-stream");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    app_call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"endeavor/set_policy","params":{"owner":"7","policy":"ask","asks":true}}"#);
    let mut events = core.connect();
    let mut events_reader = BufReader::new(events.try_clone().unwrap());
    write!(events, "GET /endeavor/events HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    event_where(&mut events_reader, |_| true);

    let post = |socket: &mut TcpStream, message: &str| {
        write!(
            socket,
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nAccept: application/json, text/event-stream\r\n\
             X-Endeavor-Session: 7\r\nX-Endeavor-Host: gpu-box\r\nContent-Length: {}\r\n\r\n{message}",
            message.len()
        )
        .unwrap();
    };
    let shell = |id: u32, meta: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"run_shell","arguments":{{"command":"echo ran"}},"_meta":{{{meta}}}}}}}"#);
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());

    // A call that asked for progress: the stream begins while it waits, with a progress notification.
    post(&mut socket, &shell(1, r#""progressToken":"p1""#));
    let head = read_until(&mut reader, "\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n") && head.contains("Content-Type: text/event-stream\r\n") && head.contains("Transfer-Encoding: chunked\r\n"), "{head}");
    let waiting = read_until(&mut reader, "\n\n");
    let ask = event_where(&mut events_reader, |e| e["asks"].as_array().is_some_and(|a| !a.is_empty()))["asks"][0].clone();
    assert!(
        waiting.ends_with(concat!(r#"data: {"jsonrpc":"2.0","method":"notifications/progress","params":{"message":"Waiting for the user's answer","progress":1,"progressToken":"p1"}}"#, "\n\n")),
        "{waiting:?}"
    );
    app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":2,"method":"endeavor/answer_run","params":{{"id":{},"allow":true,"user_ran":[]}}}}"#, ask["id"]));
    // The reply is the stream's last event, and the chunked body ends after it.
    let rest = read_until(&mut reader, "0\r\n\r\n");
    let reply = rest.split("data: ").nth(1).unwrap().split("\n\n").next().unwrap();
    let reply: serde_json::Value = serde_json::from_str(reply).unwrap();
    assert_eq!(reply["id"], 1);
    assert_eq!(serde_json::from_str::<serde_json::Value>(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()["stdout"], "ran\n");
    assert!(rest.contains("event: message\n") && rest.ends_with("\r\n0\r\n\r\n"), "{rest:?}");
    event_where(&mut events_reader, |e| e["asks"] == serde_json::json!([]));

    // The same connection carries the next call; without a progress token it waits with an SSE comment.
    post(&mut socket, &shell(2, ""));
    read_until(&mut reader, "\r\n\r\n");
    assert!(read_until(&mut reader, "\n\n").ends_with(": waiting for the user's answer\n\n"));
    let ask = event_where(&mut events_reader, |e| e["asks"].as_array().is_some_and(|a| !a.is_empty()))["asks"][0].clone();
    app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":3,"method":"endeavor/answer_run","params":{{"id":{},"allow":false,"user_ran":[]}}}}"#, ask["id"]));
    assert!(read_until(&mut reader, "0\r\n\r\n").contains(r#"\"error\":\"not_approved\""#));

    // A call that isn't held still gets plain JSON.
    let read = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_folder","arguments":{"path":"/"}}}"#;
    post(&mut socket, read);
    let (status, headers, _) = response(&mut reader);
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains(&("content-type".into(), "application/json".into())));
}

#[test]
fn in_manual_an_edit_waits_for_the_users_answer() {
    let dir = state_dir("core-manual");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    const NB: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const CELL: &str = "cccccccc-0000-0000-0000-000000000001";
    bridge.set_notebooks(vec![serde_json::json!({
        "notebook_id": NB, "path": "/n/a.jl", "cell_order": [CELL], "execution_allowed": true, "safe_preview": false, "pending_run": [],
        "cells": [{ "cell_id": CELL, "code": "x = 1", "running": false, "queued": false, "errored": false }],
    })]);
    let caller = [("X-Endeavor-Session", "7")];
    let tool = |id: u32, name: &str, arguments: serde_json::Value| {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string()
    };
    let edit = |id: u32| tool(id, "edit_cell", serde_json::json!({ "notebook_id": NB, "cell_id": CELL, "code": "x = 2" }));
    let text = |body: &str| {
        let reply: serde_json::Value = serde_json::from_str(body).unwrap();
        let text: serde_json::Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        text
    };
    let applied = || bridge.seen().iter().filter(|s| s.line.starts_with("POST /adapter") && String::from_utf8_lossy(&s.body).contains(r#""method":"apply""#)).count();
    let policy = |policy: &str, edits: bool| {
        app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"endeavor/set_policy","params":{{"owner":"7","policy":"{policy}","asks":true,"edits":{edits}}}}}"#))
    };

    let mut events = core.connect();
    let mut reader = BufReader::new(events.try_clone().unwrap());
    write!(events, "GET /endeavor/events HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    event_where(&mut reader, |_| true);
    let answer = |id: &serde_json::Value, allow: bool| app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":2,"method":"endeavor/answer_run","params":{{"id":{id},"allow":{allow},"user_ran":[]}}}}"#));
    let waiting = |e: &serde_json::Value| e["asks"].as_array().is_some_and(|a| !a.is_empty());

    policy("ask", true);
    // Reads never wait.
    assert_eq!(text(&mcp(&core, &tool(1, "read_cell", serde_json::json!({ "notebook_id": NB, "cell_id": CELL })), &caller).1)["code"], "x = 1");
    std::thread::scope(|scope| {
        // Denied: nothing reaches the notebook, and the agent hears why.
        let call = scope.spawn(|| mcp(&core, &edit(2), &caller).1);
        let ask = event_where(&mut reader, waiting)["asks"][0].clone();
        assert_eq!((&ask["tool"], &ask["arguments"]["code"]), (&"edit_cell".into(), &"x = 2".into()));
        assert!(!call.is_finished(), "waits");
        answer(&ask["id"], false);
        assert_eq!(text(&call.join().unwrap()), serde_json::json!({ "error": "not_approved", "message": "The user chose not to make this change." }));
        assert_eq!(applied(), 0, "the notebook is unchanged");
        event_where(&mut reader, |e| e["asks"] == serde_json::json!([]));

        // An edit that was to run after isn't made either.
        let run_after = tool(3, "edit_cell", serde_json::json!({ "notebook_id": NB, "cell_id": CELL, "code": "x = 2", "run_after": true }));
        let (on, from) = (&core, &caller);
        let call = scope.spawn(move || mcp(on, &run_after, from).1);
        let ask = event_where(&mut reader, waiting)["asks"][0].clone();
        answer(&ask["id"], false);
        assert_eq!(text(&call.join().unwrap())["error"], "not_approved");
        assert_eq!(applied(), 0, "the notebook is unchanged");
        event_where(&mut reader, |e| e["asks"] == serde_json::json!([]));

        // Allowed: the edit goes ahead only once the user says so.
        let call = scope.spawn(|| mcp(&core, &edit(4), &caller).1);
        let ask = event_where(&mut reader, waiting)["asks"][0].clone();
        assert_eq!(applied(), 0, "not before the answer");
        answer(&ask["id"], true);
        call.join().unwrap();
        assert_eq!(applied(), 1, "made once allowed");
        event_where(&mut reader, |e| e["asks"] == serde_json::json!([]));

        // Runs allowed for the session (policy "auto"): Manual still asks before an edit.
        policy("auto", true);
        let call = scope.spawn(|| mcp(&core, &edit(5), &caller).1);
        let ask = event_where(&mut reader, waiting)["asks"][0].clone();
        answer(&ask["id"], true);
        call.join().unwrap();
        assert_eq!(applied(), 2);
        event_where(&mut reader, |e| e["asks"] == serde_json::json!([]));
    });

    // Ask to run holds runs, not edits; neither does an app that doesn't say `edits`.
    policy("ask", false);
    mcp(&core, &edit(6), &caller);
    assert_eq!(applied(), 3, "made without asking");
    app_call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"endeavor/set_policy","params":{"owner":"7","policy":"ask","asks":true}}"#);
    mcp(&core, &edit(7), &caller);
    assert_eq!(applied(), 4, "made without asking");
    // Plan refuses edits rather than holding them.
    policy("plan", true);
    assert_eq!(text(&mcp(&core, &edit(8), &caller).1)["error"], "plan_mode");
}

#[test]
fn plan_mode_refuses_a_sessions_writes_and_runs_and_host_tools_need_a_server() {
    let dir = state_dir("core-policy");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let tool = |id: u32, name: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{{"notebook_id":"n1"}}}}}}"#);
    // A call that passes the check of its arguments, which a refusal comes before.
    let valid = |id: u32, name: &str| {
        let arguments = match name {
            "edit_cell" => r#"{"notebook_id":"n1","cell_id":"c1","code":"x"}"#,
            _ => r#"{"notebook_id":"n1","cell_id":"c1"}"#,
        };
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#)
    };
    // The core reads the notebooks' state after every tool call, to tell the app.
    let reads = || bridge.seen().iter().filter(|s| s.line.starts_with("POST /adapter")).count();
    let plan_edit = "Plan mode is read-only: `edit_cell` would change or run the notebook. Finish the plan; the user switches modes to carry it out.";
    let not_a_notebook = |id| tool_error(id, "invalid_notebook_id", "Invalid notebook ID: 'n1'\nSee `notebook_guide` for how to use these tools.");

    let set = r#"{"jsonrpc":"2.0","id":5,"method":"endeavor/set_policy","params":{"owner":"7","policy":"plan"}}"#;
    assert_eq!(app_call(&core, set), r#"{"id":5,"jsonrpc":"2.0","result":{}}"#);
    assert!(!bridge.seen().iter().any(|s| String::from_utf8_lossy(&s.body).contains("set_policy")), "the core keeps policies");

    let seven = [("X-Endeavor-Session", "7")];
    let before = reads();
    assert_eq!(mcp(&core, &tool(1, "edit_cell"), &seven).1, tool_error(1, "plan_mode", plan_edit));
    assert!(reads() > before, "the app hears the notebooks' state after it");

    // Reads pass, and so do other sessions' writes and the app's own.
    assert_eq!(mcp(&core, &valid(2, "read_cell"), &seven).1, not_a_notebook(2));
    assert_eq!(mcp(&core, &valid(3, "edit_cell"), &[("X-Endeavor-Session", "8")]).1, not_a_notebook(3));
    assert_eq!(app_call(&core, &valid(4, "edit_cell")), not_a_notebook(4));

    // Opening a notebook is refused in Plan mode only when it would run it.
    let open = |id: u32, run: bool| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"open_notebook","arguments":{{"path":"/nope.jl","run_notebook":{run}}}}}}}"#);
    let plan_open = "Plan mode is read-only: `open_notebook` would change or run the notebook. Finish the plan; the user switches modes to carry it out.";
    assert_eq!(mcp(&core, &open(20, true), &seven).1, tool_error(20, "plan_mode", plan_open));
    assert!(!mcp(&core, &open(21, false), &seven).1.contains("plan_mode"));

    // Host tools: plan mode refuses run_shell too; without a server, none run.
    let on_server = [("X-Endeavor-Session", "7"), ("X-Endeavor-Host", "gpu-box")];
    let plan_shell = "Plan mode is read-only: `run_shell` would run a command on the server. Finish the plan; the user switches modes to carry it out.";
    assert_eq!(mcp(&core, &tool(5, "run_shell"), &on_server).1, tool_error(5, "plan_mode", plan_shell));
    let not_here = |tool: &str| format!("`{tool}` is only for sessions on a server. This session runs on the user's computer: use your own file and shell tools.");
    assert_eq!(mcp(&core, &tool(6, "run_shell"), &seven).1, tool_error(6, "host_tools", &not_here("run_shell")), "the host check comes first");
    assert_eq!(app_call(&core, &tool(7, "list_folder")), tool_error(7, "host_tools", &not_here("list_folder")));

    // Null arguments are none: the plan refusal comes first. Others that aren't an object are refused.
    let null_args = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"edit_cell","arguments":null}}"#;
    assert_eq!(mcp(&core, null_args, &seven).1, tool_error(8, "plan_mode", plan_edit));
    let list_args = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"edit_cell","arguments":[]}}"#;
    assert_eq!(mcp(&core, list_args, &seven).1, tool_error(8, "invalid_argument", "arguments must be an object\nSee `notebook_guide` for how to use these tools."));

    assert_eq!(app_call(&core, &set.replace("plan", "ask")), r#"{"id":5,"jsonrpc":"2.0","result":{}}"#);
    assert_eq!(mcp(&core, &valid(9, "edit_cell"), &seven).1, not_a_notebook(9));

    // Every tool says whether it only reads, for Claude Code's own plan mode.
    let (_, body) = mcp(&core, r#"{"jsonrpc":"2.0","id":10,"method":"tools/list"}"#, &seven);
    let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
    let hint = |name: &str| reply["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"].clone();
    assert_eq!((hint("edit_cell"), hint("read_cell"), hint("allow_execution")), (false.into(), true.into(), false.into()));
    assert_eq!(hint("open_notebook"), false, "it can run the notebook");
    assert!(!bridge.seen().iter().any(|s| s.line.starts_with("POST /dispatch")), "Julia answers only the adapter's calls");
}

/// A folder of its own for a test.
fn temp_folder(name: &str) -> std::path::PathBuf {
    let dir = state_dir(name);
    dir.canonicalize().unwrap()
}

/// A session on a server, calling host tools.
struct OnServer<'a> {
    core: &'a Core,
    next: u32,
}

impl OnServer<'_> {
    /// A tool call's result (`Ok`) or error (`Err`), as the agent reads them.
    fn call(&mut self, owner: &str, name: &str, arguments: serde_json::Value) -> Result<serde_json::Value, serde_json::Value> {
        self.next += 1;
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": self.next, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
        let caller = [("X-Endeavor-Session", owner), ("X-Endeavor-Host", "gpu-box")];
        let (status, body) = mcp(self.core, &message.to_string(), &caller);
        assert_eq!(status, "HTTP/1.1 200 OK");
        let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
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
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let (_, body) = mcp(&core, list, &[("X-Endeavor-Host", "gpu-box"), ("X-Endeavor-Skills", "plugin")]);
    let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
    let tools = reply["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 29);
    assert_eq!((&names[..3], &names[26..]), (&["list_notebooks", "read_cell", "view_cell_output"][..], &["list_folder", "read_file", "run_shell"][..]), "the notebook tools, then the host tools");
    let hints: Vec<_> = tools[26..].iter().map(|t| t["annotations"]["readOnlyHint"].as_bool().unwrap()).collect();
    assert_eq!(hints, [true, true, false]);
    assert_eq!(tools[28]["inputSchema"]["required"], serde_json::json!(["command"]));
    assert!(!mcp(&core, list, &[]).1.contains("list_folder"), "not on this Mac");

    // Answered here, and the app hears the notebooks' state after each call.
    let mut server = OnServer { core: &core, next: 1 };
    let reads = || bridge.seen().iter().filter(|s| s.line.starts_with("POST /adapter")).count();
    let before = reads();
    server.run(serde_json::json!({ "command": "true" }));
    assert!(reads() > before);
}

#[test]
fn a_standalone_runtime_has_a_folder_a_fixed_port_and_host_tools_for_every_session() {
    let dir = state_dir("core-standalone");
    let bridge = FakeBridge::start(&dir);
    let folder = temp_folder("core-standalone-folder");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let env = [("ENDEAVOR_FOLDER", folder.to_str().unwrap()), ("ENDEAVOR_PORT", &port.to_string()), ("ENDEAVOR_HOST_TOOLS", "lab3")];
    let core = Core::start_with_env(&dir, &bridge, &env);
    assert_eq!(core.port, port);
    assert_eq!(read_json(&dir.join("runtime.json"))["folder"], folder.to_str().unwrap());
    let set_folder = bridge.seen().into_iter().find(|s| String::from_utf8_lossy(&s.body).contains("endeavor/set_folder")).expect("Pluto hears the folder");
    assert!(String::from_utf8_lossy(&set_folder.body).contains(&format!(r#""path":"{}""#, folder.display())));

    let (_, body) = mcp(&core, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#, &[("X-Endeavor-Skills", "plugin")]);
    assert!(body.contains("the user watches them in a web browser"), "{body}");
    let (_, body) = mcp(&core, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &[]);
    assert!(body.contains(r#""name":"run_shell""#), "host tools without X-Endeavor-Host");
    let ran = |command: &str| {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "run_shell", "arguments": { "command": command } } });
        let (_, body) = mcp(&core, &message.to_string(), &[]);
        let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
        serde_json::from_str::<serde_json::Value>(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()["stdout"].clone()
    };
    assert_eq!(ran("pwd"), format!("{}\n", folder.display()), "in the runtime's folder");
    assert_eq!(ran("echo \"$ENDEAVOR_FOLDER|$ENDEAVOR_PORT|$ENDEAVOR_HOST_TOOLS\""), "||\n", "none of the runtime's settings");
}

#[test]
fn the_runtime_says_whether_it_ends_when_idle_and_what_idle_limit_it_has() {
    let status = |core: &Core| -> serde_json::Value {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "pluto_session_status", "arguments": {} } });
        let reply: serde_json::Value = serde_json::from_str(&mcp(core, &message.to_string(), &[]).1).unwrap();
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let dir = state_dir("core-exits-when-idle");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start_with_env(&dir, &bridge, &[("ENDEAVOR_EXIT_IDLE", "1")]);
    assert_eq!(read_json(&dir.join("runtime.json"))["exits_when_idle"], true);
    let said = status(&core);
    assert_eq!((&said["exits_when_idle"], &said["idle_stop_hours"], said.get("message")), (&serde_json::json!(true), &serde_json::json!(48.0), None), "{said}");
    for (hours, shown) in [("2.5", 2.5), ("0", 0.0), ("true", 1.0), ("-3", 0.0)] {
        app_call(&core, &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"endeavor/set_idle_limit","params":{{"hours":{hours}}}}}"#));
        assert_eq!(status(&core)["idle_stop_hours"], shown, "after {hours}");
    }
    drop(core);

    let dir = state_dir("core-keeps-running");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start_with_env(&dir, &bridge, &[("ENDEAVOR_IDLE_HOURS", "6")]);
    assert_eq!(read_json(&dir.join("runtime.json"))["exits_when_idle"], false);
    let said = status(&core);
    assert_eq!((&said["exits_when_idle"], &said["idle_stop_hours"], said.get("message")), (&serde_json::json!(false), &serde_json::json!(6.0), None), "{said}");
}

#[test]
fn results_carry_a_browser_url_on_the_port_a_caller_names() {
    let dir = state_dir("core-browser-port");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let status = |caller: &[(&str, &str)]| -> serde_json::Value {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "pluto_session_status", "arguments": {} } });
        let reply: serde_json::Value = serde_json::from_str(&mcp(&core, &message.to_string(), caller).1).unwrap();
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{reply}"))).unwrap()
    };
    assert_eq!(status(&[("X-Endeavor-Session", "7")]).get("browser_url"), None, "without the header, a runtime the app or a helper started has none");
    assert_eq!(status(&[("X-Endeavor-Browser-Port", "45678")])["browser_url"], format!("http://localhost:45678/?token={TOKEN}"), "through a front's connection, whatever started the runtime");
    for bad in ["0", "65536", "-1", "http", ""] {
        assert_eq!(status(&[("X-Endeavor-Browser-Port", bad)]).get("browser_url"), None, "{bad:?}");
    }
    drop(core);

    let dir = state_dir("core-browser-port-standalone");
    let bridge = FakeBridge::start(&dir);
    let folder = temp_folder("core-browser-port-folder");
    let core = Core::start_with_env(&dir, &bridge, &[("ENDEAVOR_FOLDER", folder.to_str().unwrap())]);
    let own = core.port;
    let status = |caller: &[(&str, &str)]| -> serde_json::Value {
        let message = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "pluto_session_status", "arguments": {} } });
        let reply: serde_json::Value = serde_json::from_str(&mcp(&core, &message.to_string(), caller).1).unwrap();
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    assert_eq!(status(&[])["browser_url"], format!("http://localhost:{own}/?token={TOKEN}"));
    assert_eq!(status(&[("X-Endeavor-Browser-Port", "45678")])["browser_url"], format!("http://localhost:45678/?token={TOKEN}"), "the caller's port wins");
    assert_eq!(status(&[("X-Endeavor-Browser-Port", "0")])["browser_url"], format!("http://localhost:{own}/?token={TOKEN}"), "a bad one is ignored");
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
    let mut server = OnServer { core: &core, next: 0 };
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
    let mut server = OnServer { core: &core, next: 0 };
    let folder = temp_folder("core-host-shell-cwd");

    let ran = server.run(serde_json::json!({ "command": "echo out; echo err >&2; pwd; exit 3", "cwd": folder }));
    assert_eq!(ran, serde_json::json!({ "exit_code": 3, "stdout": format!("out\n{}\n", folder.display()), "stderr": "err\n", "timed_out": false, "cwd": folder }));
    assert_eq!(server.run(serde_json::json!({ "command": "pwd" }))["stdout"], format!("{}\n", home.display()), "home by default");
    let env = server.run(serde_json::json!({ "command": "echo \"$JULIA_DEPOT_PATH|$ENDEAVOR_TOKEN|$ENDEAVOR_LAUNCHER$ENDEAVOR_BUILD\"" }));
    assert_eq!(env["stdout"], "/opt/depot:||\n", "Julia's environment, without the runtime's secrets");

    // A session's own folder is where it runs by default.
    let set = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"endeavor/set_session_folder","params":{{"owner":"8","folder":"{}"}}}}"#, folder.display());
    assert_eq!(app_call(&core, &set), r#"{"id":1,"jsonrpc":"2.0","result":{}}"#);
    assert_eq!(server.call("8", "run_shell", serde_json::json!({ "command": "pwd" })).unwrap()["stdout"], format!("{}\n", folder.display()));
    assert_eq!(server.call("8", "run_shell", serde_json::json!({ "command": "pwd", "cwd": "~" })).unwrap()["stdout"], format!("{}\n", home.display()));
    app_call(&core, &set.replace(&folder.display().to_string(), ""));
    assert_eq!(server.call("8", "run_shell", serde_json::json!({ "command": "pwd" })).unwrap()["stdout"], format!("{}\n", home.display()));

    let error = |kind: &str, message: &str| Err(serde_json::json!({ "error": kind, "message": message }));
    assert_eq!(server.call("", "run_shell", serde_json::json!({ "command": "  " })), error("invalid_argument", "command is empty"));
    assert_eq!(server.call("", "run_shell", serde_json::json!({})), error("invalid_argument", "`run_shell` needs `command`."));
    assert_eq!(server.call("", "run_shell", serde_json::json!({ "command": 5 })), error("invalid_argument", "command must be a string"));
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
    // The core reads notebooks from Julia on its own; those aren't passed-on requests.
    let passed_on = || bridge.seen().iter().filter(|s| !s.line.starts_with("POST /adapter") && !s.line.starts_with("GET /notifications")).count() + bridge.pluto_seen().len();
    let seen_before = passed_on();
    for route in ["POST /mcp", "POST /endeavor/call", "GET /endeavor/events", "POST /endeavor/dispatch", "GET /nope", "GET /edit?id=1"] {
        let request = |headers: &str| format!("{route} HTTP/1.1\r\n{headers}Content-Length: 2\r\n\r\n{{}}");
        assert_eq!(ask(&request("Host: 127.0.0.1\r\n")), refused("401 Unauthorized", "unauthorized"), "{route}");
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\n{}", auth.replace('0', "1")))), refused("401 Unauthorized", "unauthorized"));
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\n{}", auth.replace("\r\n", "0\r\n")))), refused("401 Unauthorized", "unauthorized"));
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\nOrigin: https://example.com\r\n{auth}"))), refused("403 Forbidden", "browser_origin_refused"));
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1\r\nOrigin: null\r\n{auth}"))), refused("403 Forbidden", "browser_origin_refused"));
        assert_eq!(ask(&request(&format!("Host: evil.example:80\r\n{auth}"))), refused("403 Forbidden", "host_not_loopback"), "{route}");
        assert_eq!(ask(&request(&format!("Host: 127.0.0.1.evil.example\r\n{auth}"))), refused("403 Forbidden", "host_not_loopback"));
        assert_eq!(ask(&request(&auth)), refused("403 Forbidden", "host_not_loopback"));
    }
    assert_eq!(passed_on(), seen_before, "nothing refused reaches Julia or Pluto");
    for host in ["localhost", "[::1]:9", "127.0.0.1:9"] {
        let request = format!("POST /mcp HTTP/1.1\r\nHost: {host}\r\n{auth}Content-Length: 2\r\n\r\n{{}}");
        assert_eq!(ask(&request).0, "HTTP/1.1 202 Accepted", "{host} is loopback");
    }

    // A refused request leaves the connection for the next one.
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    write!(socket, "POST /endeavor/call HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\n{{}}").unwrap();
    assert_eq!(response(&mut reader).0, "HTTP/1.1 401 Unauthorized");
    write!(socket, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}\r\n").unwrap();
    assert_eq!(response(&mut reader).2, "Pluto: GET / HTTP/1.1");
}

#[test]
fn a_browser_gets_in_with_its_link_and_then_reaches_only_plutos_page() {
    let dir = state_dir("core-browser");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let ask = |request: &str| {
        let mut socket = core.connect();
        socket.write_all(request.as_bytes()).unwrap();
        response(&mut BufReader::new(socket))
    };
    let host = format!("Host: 127.0.0.1:{}\r\n", core.port);
    let (status, headers, _) = ask(&format!("GET /edit?id=n1&token={TOKEN} HTTP/1.1\r\n{host}\r\n"));
    assert_eq!(status, "HTTP/1.1 303 See Other");
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    assert_eq!(header("location"), Some("/edit?id=n1"), "the same page, without the token");
    let set = header("set-cookie").unwrap();
    assert!(set.ends_with(&format!("={TOKEN}; Path=/; HttpOnly; SameSite=Strict")), "{set}");
    let cookie = format!("Cookie: {}\r\n", set.split(';').next().unwrap());
    assert!(bridge.pluto_seen().is_empty(), "the visit with the token never reached Pluto");

    let (status, headers, body) = ask(&format!("GET /edit?id=n1 HTTP/1.1\r\n{host}Cookie: theme=dark\r\n{cookie}Sec-Fetch-Site: none\r\n\r\n"));
    assert_eq!((status.as_str(), body.as_str()), ("HTTP/1.1 200 OK", "Pluto: GET /edit?id=n1 HTTP/1.1"));
    let cookies: Vec<&str> = headers.iter().filter(|(n, _)| n == "set-cookie").map(|(_, v)| v.as_str()).collect();
    assert_eq!(cookies, ["theme=dark"], "Pluto's own secret never reaches the browser");
    assert_eq!(bridge.pluto_seen()[0].header("Cookie"), Some("secret=s3cret"), "only Pluto's secret reaches Pluto");

    let refused = |status: &str, error: &str| (format!("HTTP/1.1 {status}"), format!(r#"{{"error":"{error}"}}"#));
    let ask = |request: &str| {
        let (status, _, body) = ask(request);
        (status, body)
    };
    for route in ["POST /mcp", "POST /endeavor/call", "GET /endeavor/events"] {
        assert_eq!(ask(&format!("{route} HTTP/1.1\r\n{host}{cookie}Content-Length: 2\r\n\r\n{{}}")), refused("401 Unauthorized", "unauthorized"), "{route}");
    }
    let origin = format!("Origin: http://127.0.0.1:{}\r\n", core.port + 1);
    assert_eq!(ask(&format!("GET /edit?id=n1 HTTP/1.1\r\n{host}{origin}{cookie}\r\n")), refused("403 Forbidden", "browser_origin_refused"), "another runtime's page");
    assert_eq!(ask(&format!("GET /?token=nope HTTP/1.1\r\n{host}\r\n")), refused("401 Unauthorized", "unauthorized"));
    assert_eq!(bridge.pluto_seen().len(), 1);
}

#[test]
fn passes_a_websocket_through_both_ways_until_either_side_closes() {
    let dir = state_dir("core-websocket");
    let bridge = FakeBridge::start(&dir);
    let core = Core::start(&dir, &bridge);
    let mut socket = core.connect();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let (_, headers, _) = {
        let mut visit = core.connect();
        write!(visit, "GET /?token={TOKEN} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", core.port).unwrap();
        response(&mut BufReader::new(visit))
    };
    let cookie = headers.iter().find(|(n, _)| n == "set-cookie").unwrap().1.split(';').next().unwrap().to_owned();
    write!(
        socket,
        "GET /channels HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nCookie: {cookie}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        port = core.port
    )
    .unwrap();
    let head = read_until(&mut reader, "\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{head}");
    assert_eq!(bridge.pluto_seen()[0].header("Cookie"), Some("secret=s3cret"));
    for message in ["first frame\n", "second\n"] {
        socket.write_all(message.as_bytes()).unwrap();
        assert_eq!(read_until(&mut reader, "\n"), message, "back from Pluto");
    }
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut writer = socket.try_clone().unwrap();
    let sent = big.clone();
    let send = std::thread::spawn(move || writer.write_all(&sent).unwrap());
    let mut back = vec![0; big.len()];
    reader.read_exact(&mut back).unwrap();
    send.join().unwrap();
    assert!(back == big, "300 KB both ways at once");

    socket.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(bridge.socket_closed(), "the client closing closes Pluto's end");
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
