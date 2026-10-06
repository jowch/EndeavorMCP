//! The link over real ssh and real Julia: `endeavor link` signs in with keys,
//! installs the helper into a folder of the test's own, starts the runtime
//! there, and the agent's MCP calls and the browser's page reach it through the
//! link's port, until a stop and a quit leave nothing running. It's ignored by
//! default and runs only when `ENDEAVOR_TEST_SSH_HOST` names a host that this
//! user can `ssh` to with a key (`localhost` is one):
//!
//!     ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_link -- --ignored --nocapture
//!
//! Julia is `ENDEAVOR_E2E_JULIA` if set, else the app's own, else `julia` on the
//! PATH, and it has to be at the same path on the host. Its depot is
//! `e2e_client`'s, in `target/tmp/e2e-client/depot`, so run that test first or
//! expect several minutes. The link's records and the machines file are under
//! `target/tmp/e2e-link`, and the helper's install and state folders too.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{find_julia, pid_alive, wait_for};
use endeavor_mcp::client::{MachinesFile, Server};
use endeavor_mcp::link::{Link, Spawn, State, ensure_with};
use serde_json::{Value, json};

/// A failed step leaves no link and no Julia behind.
struct Ends {
    state: PathBuf,
    links: PathBuf,
}

impl Drop for Ends {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.links) {
            for entry in entries.flatten() {
                if let Some(pid) = std::fs::read_to_string(entry.path().join("link.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).filter(|&p| p > 1) {
                    // SAFETY: plain syscall, on the link this test started.
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
            }
        }
        if let Some(pid) = std::fs::read_to_string(self.state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).filter(|&p| p > 1) {
            // SAFETY: plain syscall, on the runtime this test started: the core, Julia and its workers are one process group.
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        }
    }
}

/// One request to `port`: the status line, the head and the body.
fn http(port: u16, request: &str) -> (String, String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    socket.write_all(request.as_bytes()).unwrap();
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply).unwrap();
    let reply = String::from_utf8_lossy(&reply).into_owned();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.lines().next().unwrap_or_default().to_owned(), head.to_owned(), body.to_owned())
}

/// A tool call as the agent makes it, through the link's port. Its result, as the tool's JSON.
fn tool(port: u16, token: &str, id: u64, name: &str, arguments: Value) -> Value {
    let message = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string();
    let request = format!(
        "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Endeavor-Session: e2e-link\r\nX-Endeavor-Browser-Port: {port}\r\nContent-Length: {}\r\n\r\n{message}",
        message.len()
    );
    let (status, _, body) = http(port, &request);
    assert_eq!(status, "HTTP/1.1 200 OK", "{body}");
    let reply: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{reply}"));
    assert_eq!(reply["result"]["isError"], false, "{text}");
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn wait_status(link: &Link, what: &str, limit: Duration, done: impl Fn(&endeavor_mcp::link::Status) -> bool) -> endeavor_mcp::link::Status {
    let deadline = Instant::now() + limit;
    loop {
        let status = link.status().expect("status");
        if done(&status) {
            return status;
        }
        assert!(status.state != State::Failed, "failed while waiting for {what}: {:?}", status.error);
        assert!(Instant::now() < deadline, "timed out waiting for {what}; the link says {status:#?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[ignore = "needs ENDEAVOR_TEST_SSH_HOST and starts real Julia, several minutes the first time: ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_link -- --ignored"]
fn the_link_over_real_ssh() {
    let Ok(host) = std::env::var("ENDEAVOR_TEST_SSH_HOST") else {
        eprintln!("SKIPPED: ENDEAVOR_TEST_SSH_HOST isn't set. Name a host this user can ssh to with a key, such as localhost.");
        return;
    };
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return;
    };
    let started = Instant::now();
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-link");
    let (root, state, notebooks) = (work.join("root"), work.join("state"), work.join("notebooks"));
    let depot = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-client/depot");
    let links = work.join("state-home/endeavor/links");
    drop(Ends { state: state.clone(), links: links.clone() });
    let _ = std::fs::remove_dir_all(&work);
    for folder in [&root, &state, &notebooks, &depot, &work.join("config")] {
        std::fs::create_dir_all(folder).unwrap();
    }
    let canon = |p: &Path| p.canonicalize().unwrap();
    let (root, state, depot, notebooks, work) = (canon(&root), canon(&state), canon(&depot), canon(&notebooks), canon(&work));
    let links = work.join("state-home/endeavor/links");
    let depot_path = match &app {
        Some(app) => format!("{}:{}:", depot.display(), app.join("depot").display()),
        None => format!("{}:", depot.display()),
    };
    let _ends = Ends { state: state.clone(), links };
    eprintln!("host: {host}, julia: {}", julia.display());

    let machines = MachinesFile::at(work.join("config/endeavor/machines.json"));
    machines.save(Server { id: "e2e-link".into(), name: host.clone(), ssh_host: host.clone(), julia: Some(julia.display().to_string()), ..Default::default() }).unwrap();
    // HOME stays the user's own, where ssh finds its keys; the link's files and the machines file are ours.
    let env = [
        ("XDG_STATE_HOME", work.join("state-home").display().to_string()),
        ("XDG_CONFIG_HOME", work.join("config").display().to_string()),
        ("ENDEAVOR_LINK_ROOT", root.display().to_string()),
        ("ENDEAVOR_LINK_STATE", state.display().to_string()),
        ("ENDEAVOR_LINK_DEPOT", depot_path),
        ("ENDEAVOR_LINK_IDLE_SECS", "3600".into()),
    ];
    let spawn = Spawn { exe: PathBuf::from(env!("CARGO_BIN_EXE_endeavor")), env: env.into_iter().map(|(k, v)| (k.to_owned(), v)).collect() };

    let link = ensure_with(&spawn, "e2e-link").expect("a link");
    assert_eq!(ensure_with(&spawn, "e2e-link").unwrap(), link, "a second front reuses it");
    let looked = wait_status(&link, "the helper missing", Duration::from_secs(120), |s| s.state == State::NeedsInstall);
    assert!(looked.needs_install.is_some() && !root.join(endeavor_mcp::embedded::BUILD_VERSION).exists(), "a look installs nothing");
    link.install().expect("install");
    let status = wait_status(&link, "connected", Duration::from_secs(120), |s| s.state == State::Connected);
    let hello = status.hello.expect("hello");
    assert!(!hello.node.is_empty() && hello.uploads);
    assert_eq!(hello.helper_installed, Some(true));
    let installed = root.join(endeavor_mcp::embedded::BUILD_VERSION);
    assert!(installed.join("endeavor").is_file() && installed.join("runtime/boot.jl").is_file(), "the helper is installed in {}", installed.display());
    eprintln!("[{:?}] connected to {}", started.elapsed(), hello.node);

    link.start(None).expect("start");
    let status = wait_status(&link, "ready", Duration::from_secs(1200), |s| s.state == State::Ready);
    let runtime = status.runtime.expect("a runtime");
    eprintln!("[{:?}] ready on {}, pid {}", started.elapsed(), runtime.node, runtime.pid);
    assert!(!runtime.reattached, "a runtime was already running in {}", state.display());
    assert!(runtime.token.len() >= 32 && runtime.page_url.contains(&runtime.token));

    // The agent's calls through the link's port: a notebook, and the links it gets.
    let (port, token) = (runtime.port, runtime.token.as_str());
    let path = notebooks.join("analysis.jl");
    let created = tool(port, token, 1, "new_notebook", json!({ "path": path.display().to_string() }));
    let notebook = created["notebook_id"].as_str().expect("a notebook").to_owned();
    assert_eq!(created["browser_url"], json!(format!("http://localhost:{port}/edit?id={notebook}&token={token}")), "{created}");
    let session = tool(port, token, 2, "pluto_session_status", json!({}));
    assert_eq!(session["browser_url"], json!(format!("http://localhost:{port}/?token={token}")), "{session}");
    eprintln!("[{:?}] tool calls through the link's port", started.elapsed());

    // The page, as a browser reaches it: the link sets the cookie, then the page loads.
    let (status, head, _) = http(port, &format!("GET /edit?id={notebook}&token={token} HTTP/1.0\r\nHost: localhost:{port}\r\n\r\n"));
    assert_eq!(status, "HTTP/1.1 303 See Other", "{head}");
    let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}"));
    let cookie = set.split(';').next().unwrap();
    let (status, head, page) = http(port, &format!("GET /edit?id={notebook} HTTP/1.0\r\nHost: localhost:{port}\r\nCookie: {cookie}\r\nSec-Fetch-Site: none\r\n\r\n"));
    assert_eq!(status, "HTTP/1.1 200 OK", "{head}");
    assert!(page.contains("Pluto"), "{}", &page[..page.len().min(300)]);
    eprintln!("[{:?}] the page answers", started.elapsed());

    link.stop().expect("stop");
    wait_for("the runtime to end", || !pid_alive(runtime.pid as i32));
    let status = link.status().unwrap();
    assert_eq!((status.state, status.runtime), (State::Connected, None));
    link.quit().expect("quit");
    wait_for("the link to end", || !pid_alive(link.pid as i32));
    assert!(!work.join("state-home/endeavor/links/e2e-link/link.json").exists());
    let helpers = Command::new("pgrep").arg("-f").arg("--").arg(format!("connect --state-dir {}", state.display())).output().unwrap();
    wait_for("the helper to go", || Command::new("pgrep").arg("-f").arg("--").arg(format!("connect --state-dir {}", state.display())).output().unwrap().stdout.is_empty());
    drop(helpers);
    assert!(!state.join("runtime.json").exists());
    eprintln!("[{:?}] stopped and quit; nothing left", started.elapsed());
}
