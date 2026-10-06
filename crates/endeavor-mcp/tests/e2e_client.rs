//! The client library over real ssh and real Julia: sign in with keys
//! (`Auth::Batch`), install the helper into a folder of the test's own, start the
//! runtime there, reach it through the local listener's port with the agent's
//! MCP calls, then stop it. It's ignored by default and runs only when
//! `ENDEAVOR_TEST_SSH_HOST` names a host that this user can `ssh` to with a key
//! (`localhost` is one):
//!
//!     ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_client -- --ignored --nocapture
//!
//! Julia is `ENDEAVOR_E2E_JULIA` if set, else the app's own, else `julia` on the
//! PATH, and it has to be at the same path on the host. Its depot is the test's
//! own in the target folder, which keeps what Julia installs and compiles
//! between runs, so the first run takes several minutes.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::find_julia;
use endeavor_mcp::client::{Auth, Cancel, Event, Listener, Options, Server, Transport, connect, start};
use serde_json::{Value, json};

/// End the runtime recorded in `state`, if one runs: the core, Julia and its
/// notebook workers are one process group.
fn end_runtime(state: &Path) {
    let Ok(text) = std::fs::read_to_string(state.join("runtime.json")) else { return };
    if let Some(pid) = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["pid"].as_i64()).filter(|&p| p > 1) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    }
}

/// A failed step leaves no Julia behind.
struct EndsRuntime(PathBuf);

impl Drop for EndsRuntime {
    fn drop(&mut self) {
        end_runtime(&self.0);
    }
}

/// POST `body` to `/mcp` on the listener's port as the agent does: the status line and the body.
fn mcp(port: u16, token: &str, message: &Value) -> (String, Value) {
    let body = message.to_string();
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    write!(
        socket,
        "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Endeavor-Session: 1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.lines().next().unwrap_or_default().to_owned(), serde_json::from_str(body).unwrap_or(Value::Null))
}

#[test]
#[ignore = "needs ENDEAVOR_TEST_SSH_HOST and starts real Julia, several minutes the first time: ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_client -- --ignored"]
fn a_runtime_over_real_ssh() {
    let Ok(host) = std::env::var("ENDEAVOR_TEST_SSH_HOST") else {
        eprintln!("SKIPPED: ENDEAVOR_TEST_SSH_HOST isn't set. Name a host this user can ssh to with a key, such as localhost.");
        return;
    };
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return;
    };
    let started = Instant::now();
    eprintln!("host: {host}, julia: {}", julia.display());
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-client");
    let (root, state, depot) = (work.join("root"), work.join("state"), work.join("depot"));
    end_runtime(&state);
    for folder in [&root, &state] {
        let _ = std::fs::remove_dir_all(folder);
    }
    std::fs::create_dir_all(&depot).unwrap();
    let (root, state, depot) = (root.canonicalize().unwrap_or(root), state.canonicalize().unwrap_or(state), depot.canonicalize().unwrap());
    // Ours first, where Julia writes; the app's behind it, read-only; the defaults last.
    let depot_path = match &app {
        Some(app) => format!("{}:{}:", depot.display(), app.join("depot").display()),
        None => format!("{}:", depot.display()),
    };
    let _ends = EndsRuntime(state.clone());

    let server = Server { ssh_host: host.clone(), julia: Some(julia.display().to_string()), ..Default::default() };
    let helper = |_: &str, _: &str| Ok(PathBuf::from(env!("CARGO_BIN_EXE_endeavor")));
    let options = Options { auth: Auth::Batch, root: root.display().to_string(), state: state.display().to_string(), depot: depot_path, exit_idle: false, helper: &helper };
    let (events_tx, events) = mpsc::channel();
    let on = move |event: Event| drop(events_tx.send(event));

    let (channel, hello) = connect(&server, &Transport::for_server(&server), &options, &Cancel::default(), &on).expect("connect over ssh");
    eprintln!("[{:?}] hello from {}, home {}", started.elapsed(), hello.node, hello.home.display());
    assert!(!hello.node.is_empty() && hello.uploads);
    let installed = root.join(endeavor_mcp::embedded::BUILD_VERSION);
    assert!(installed.join("endeavor").is_file() && installed.join("runtime/boot.jl").is_file(), "the helper is installed in {}", installed.display());
    assert!(matches!(events.try_iter().find(|e| matches!(e, Event::Helper { .. })), Some(Event::Helper { installed: true })));

    let listener = Listener::start(&host).unwrap();
    let (lost_tx, lost) = mpsc::channel();
    let (ready_tx, ready) = mpsc::channel();
    let channel = std::sync::Arc::new(channel);
    std::thread::spawn({
        let (channel, listener) = (channel.clone(), listener.clone());
        move || drop(ready_tx.send(start(&channel, &listener, None, &on, move |notice| drop(lost_tx.send(notice)))))
    });
    let runtime = ready.recv_timeout(Duration::from_secs(1200)).expect("the runtime is ready in time").expect("start");
    eprintln!("[{:?}] ready on {}, pid {}", started.elapsed(), runtime.node, runtime.pid);
    assert!(!runtime.reattached, "a runtime was already running in {}", state.display());
    assert_eq!(runtime.port, listener.port());
    assert_eq!(runtime.mcp_url, format!("http://127.0.0.1:{}/mcp", listener.port()));
    assert!(runtime.page_url.contains(&runtime.token) && runtime.token.len() >= 32);

    let (status, init) = mcp(runtime.port, &runtime.token, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e-client", "version": "0" } } }));
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(init["result"]["serverInfo"]["name"], "endeavor-runtime", "{init}");
    let (status, listed) = mcp(runtime.port, &runtime.token, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }));
    assert_eq!(status, "HTTP/1.1 200 OK");
    let names: Vec<&str> = listed["result"]["tools"].as_array().unwrap_or_else(|| panic!("{listed}")).iter().filter_map(|t| t["name"].as_str()).collect();
    for tool in ["list_notebooks", "new_notebook", "add_cell", "execute_cell"] {
        assert!(names.contains(&tool), "tools/list has {tool}: {names:?}");
    }
    eprintln!("[{:?}] {} tools through the listener", started.elapsed(), names.len());
    let (status, _) = mcp(runtime.port, "wrong", &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {} }));
    assert_eq!(status, "HTTP/1.1 401 Unauthorized", "the token is checked");

    channel.stop().expect("stop");
    let deadline = Instant::now() + Duration::from_secs(30);
    // SAFETY: signal 0 only checks that the process exists.
    while unsafe { libc::kill(runtime.pid as i32, 0) } == 0 {
        assert!(Instant::now() < deadline, "the runtime (pid {}) is still running after stop", runtime.pid);
        std::thread::sleep(Duration::from_millis(100));
    }
    channel.detach();
    assert!(channel.closed().is_none(), "the helper ended because the client let it go");
    assert!(lost.try_recv().is_err(), "stopping on purpose is no notice");
    eprintln!("[{:?}] stopped and detached", started.elapsed());
}
