//! The link process (`endeavor link`) against the helper binary, with a local
//! `sh` standing in for ssh (`ENDEAVOR_LINK_SHELL`) and a stand-in Julia under
//! the real core, so neither sshd nor Julia is needed: finding or starting
//! one link for a machine, starting and stopping the runtime through it, the
//! runtime reached through the listener's port, getting the connection back,
//! and each way the link ends. Every folder is under `target/tmp`.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{FakeBridge, TOKEN, pid_alive, serving_julia, wait_for};
use endeavor_mcp::client::{MachinesFile, Server};
use endeavor_mcp::link::{Link, Spawn, State, Status, ensure_with};
use serde_json::{Value, json};

/// One machine called `lab-NAME` whose helper, runtime and link all live in a folder of the test's own.
struct Place {
    dir: PathBuf,
    id: String,
    /// The helper's state folder, where its runtime keeps `runtime.json`.
    state: PathBuf,
    spawn: Spawn,
    _bridge: FakeBridge,
}

impl Place {
    fn new(name: &str) -> Place {
        Place::with(name, &[])
    }

    /// With more variables for the link, or other values for those the test sets.
    fn with(name: &str, more: &[(&str, &str)]) -> Place {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("link-{name}"));
        // A link or a runtime of an earlier run that failed halfway.
        end_leftovers(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("runtime-state");
        std::fs::create_dir_all(&state).unwrap();
        let bridge = FakeBridge::start(&state);
        let julia = serving_julia(&state, &bridge);
        std::fs::write(state.join("token"), TOKEN).unwrap();
        let id = format!("lab-{name}");
        let config = dir.join("config");
        MachinesFile::at(config.join("endeavor/machines.json"))
            .save(Server { id: id.clone(), name: "lab".into(), ssh_host: "lab".into(), julia: Some(julia.display().to_string()), ..Default::default() })
            .unwrap();
        let path = |name: &str| dir.join(name).display().to_string();
        let mut env: Vec<(String, String)> = [
            ("HOME", path("home")),
            ("XDG_STATE_HOME", path("state-home")),
            ("XDG_CONFIG_HOME", path("config")),
            ("ENDEAVOR_LINK_SHELL", "1".into()),
            ("ENDEAVOR_LINK_ROOT", path("root")),
            ("ENDEAVOR_LINK_STATE", state.display().to_string()),
            ("ENDEAVOR_LINK_DEPOT", path("depot")),
            ("ENDEAVOR_LINK_IDLE_SECS", "3600".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        for (name, value) in more {
            env.retain(|(n, _)| n != name);
            env.push((name.to_string(), value.replace("{dir}", &dir.display().to_string())));
        }
        let spawn = Spawn { exe: PathBuf::from(env!("CARGO_BIN_EXE_endeavor")), env };
        Place { dir, id, state, spawn, _bridge: bridge }
    }

    fn ensure(&self) -> Link {
        ensure_with(&self.spawn, &self.id).expect("a link")
    }

    fn record(&self) -> PathBuf {
        self.dir.join("state-home/endeavor/links").join(&self.id).join("link.json")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.record().with_file_name("link.log")).unwrap_or_default()
    }

    /// The pids of the helper (`endeavor connect`) of this machine.
    fn helpers(&self) -> Vec<i32> {
        pids(&format!("connect --state-dir {}", self.state.display()))
    }

    /// The pid of the runtime's core, once it runs.
    fn runtime(&self) -> Option<i32> {
        let state: Value = serde_json::from_str(&std::fs::read_to_string(self.state.join("runtime.json")).ok()?).ok()?;
        state["pid"].as_i64().map(|p| p as i32)
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        end_leftovers(&self.dir);
    }
}

fn pids(pattern: &str) -> Vec<i32> {
    let found = Command::new("pgrep").arg("-f").arg("--").arg(pattern).output().unwrap();
    String::from_utf8_lossy(&found.stdout).split_whitespace().filter_map(|p| p.parse().ok()).collect()
}

/// End what a test of `dir` started: its link, then its runtime (the core, Julia and its workers are one process group).
fn end_leftovers(dir: &Path) {
    let find = |glob: &str| -> Option<Value> {
        let links = dir.join("state-home/endeavor/links");
        let entry = std::fs::read_dir(links).ok()?.flatten().next()?;
        serde_json::from_str(&std::fs::read_to_string(entry.path().join(glob)).ok()?).ok()
    };
    if let Some(pid) = find("link.json").and_then(|r| r["pid"].as_i64()).filter(|&p| p > 1) {
        // SAFETY: plain syscall, on the link this test started.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        wait_for("the link to end", || !pid_alive(pid as i32));
    }
    let state = dir.join("runtime-state");
    if let Some(pid) = std::fs::read_to_string(state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).filter(|&p| p > 1) {
        // SAFETY: plain syscall, on the runtime this test started.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    }
}

/// Poll the link's status until `done`, which it has to within 40 s.
fn wait_status(link: &Link, what: &str, done: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let status = link.status().expect("status");
        if done(&status) {
            return status;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; the link says {status:#?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ready(link: &Link) -> Status {
    wait_status(link, "ready", |s| s.state == State::Ready)
}

/// One request to `port`: the status code and the body.
fn http(port: u16, request: &str) -> (u16, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    socket.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    let _ = socket.read_to_string(&mut reply);
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (code, body.to_owned())
}

/// A tool call as the agent makes it, through the listener's `port`. The tool's result.
fn tool(port: u16, token: &str, name: &str, browser_port: Option<u16>) -> Value {
    let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": {} } }).to_string();
    let browser = browser_port.map(|p| format!("X-Endeavor-Browser-Port: {p}\r\n")).unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Endeavor-Session: 1\r\n{browser}Content-Length: {}\r\n\r\n{message}",
        message.len()
    );
    let (code, body) = http(port, &request);
    assert_eq!(code, 200, "{body}");
    let reply: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{reply}"));
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

#[test]
fn one_link_serves_every_front_and_reaches_ready() {
    let place = Place::new("ready");
    // Fronts that start together get one link.
    let links: Vec<Link> = std::thread::scope(|scope| {
        let (spawn, id) = (&place.spawn, &place.id);
        let starts: Vec<_> = (0..4).map(|_| scope.spawn(|| ensure_with(spawn, id).expect("a link"))).collect();
        starts.into_iter().map(|s| s.join().unwrap()).collect()
    });
    assert!(links.iter().all(|l| l == &links[0]), "{links:?}");
    let link = links[0].clone();
    assert!(pid_alive(link.pid as i32));
    assert_eq!(place.ensure(), link, "a later front reuses it");
    assert_eq!(pids("link --machine lab-ready").len(), 1, "one process");
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(place.record()).unwrap()).unwrap();
    assert_eq!((on_disk["pid"].as_u64(), on_disk["port"].as_u64(), on_disk["token"].as_str()), (Some(link.pid as u64), Some(link.port as u64), Some(link.token.as_str())));
    assert_eq!(on_disk["build"], endeavor_mcp::embedded::BUILD_VERSION);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(place.record()).unwrap().permissions().mode() & 0o777, 0o600);

    // Connected, with what the helper said, and no runtime until it is asked for.
    let status = wait_status(&link, "connected", |s| s.state == State::Connected);
    let hello = status.hello.clone().expect("the helper's hello");
    assert!(!hello.node.is_empty() && hello.uploads);
    assert_eq!((hello.helper_installed, hello.os.as_deref()), (Some(true), Some("Linux")));
    assert_eq!((status.machine.as_str(), status.name.as_str(), status.runtime.clone()), (place.id.as_str(), "lab", None));
    assert!(place.runtime().is_none());

    let asked = link.start(None).expect("start");
    assert!(matches!(asked.state, State::Connected | State::Starting), "answered at once: {asked:?}");
    let status = ready(&link);
    let runtime = status.runtime.clone().expect("the runtime");
    assert_eq!(runtime.token, TOKEN);
    assert!(!runtime.reattached && runtime.pid as i32 == place.runtime().unwrap());
    assert_eq!(runtime.page_url, format!("http://127.0.0.1:{}/?token={TOKEN}", runtime.port));
    assert_eq!(runtime.mcp_url, format!("http://127.0.0.1:{}/mcp", runtime.port));
    assert_ne!(runtime.port, link.port, "the control port isn't the listener's");
    assert_eq!(status.hello.unwrap().julia.map(|j| j.version), Some("1.12.0".into()));

    // A start with the runtime attached is not an error and changes nothing.
    let again = link.start(None).expect("start again");
    assert_eq!((again.state, again.runtime.map(|r| r.pid)), (State::Ready, Some(runtime.pid)));

    // The agent's call through the listener, with the browser port it was given.
    let status = tool(runtime.port, &runtime.token, "pluto_session_status", Some(runtime.port));
    assert_eq!(status["browser_url"], format!("http://localhost:{}/?token={TOKEN}", runtime.port), "{status}");
    assert!(tool(runtime.port, &runtime.token, "pluto_session_status", None).get("browser_url").is_none());
    let wrong = "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer wrong\r\nContent-Length: 2\r\n\r\n{}";
    assert_eq!(http(runtime.port, wrong).0, 401, "the runtime checks the token");
}

#[test]
fn stopping_ends_the_runtime_but_not_the_link_and_a_start_after_it_works() {
    let place = Place::new("stop");
    let link = place.ensure();
    link.start(None).unwrap();
    let first = ready(&link).runtime.unwrap();
    link.stop().expect("stop");
    let status = link.status().unwrap();
    assert_eq!((status.state, status.runtime), (State::Connected, None));
    assert!(!pid_alive(first.pid as i32), "the runtime is gone");
    assert!(!place.helpers().is_empty(), "the link stays connected");
    // The listener says what to do, in the link's words.
    let said = tool(first.port, &first.token, "list_notebooks", None);
    assert!(said.as_str().is_some_and(|text| text.contains("Call use_machine")), "{said}");

    link.start(None).unwrap();
    let second = ready(&link).runtime.unwrap();
    assert_eq!(second.port, first.port, "the same listener");
    assert!(!second.reattached && second.pid != first.pid);
    assert!(tool(second.port, &second.token, "pluto_session_status", Some(second.port)).get("browser_url").is_some());
    // Nothing runs, so a stop says why it can't.
    link.stop().unwrap();
    assert!(link.stop().is_ok(), "the helper has nothing to stop and says so");
}

#[test]
fn the_connection_comes_back_on_the_same_port() {
    let place = Place::new("reconnect");
    let link = place.ensure();
    link.start(None).unwrap();
    let before = ready(&link).runtime.unwrap();
    let helpers = place.helpers();
    assert!(!helpers.is_empty());
    for pid in &helpers {
        // SAFETY: plain syscall, on the helper this test's link started.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    wait_for("the helper to change", || place.helpers().iter().all(|p| !helpers.contains(p)) && !place.helpers().is_empty());
    let after = ready(&link).runtime.unwrap();
    assert_eq!((after.port, after.pid, after.reattached), (before.port, before.pid, true), "the same listener and the same runtime");
    assert!(tool(after.port, &after.token, "pluto_session_status", Some(after.port)).get("browser_url").is_some());
    assert!(pid_alive(before.pid as i32));
}

#[test]
fn a_runtime_that_ended_while_disconnected_is_not_started_again() {
    let place = Place::new("ended-away");
    let link = place.ensure();
    link.start(None).unwrap();
    let before = ready(&link).runtime.unwrap();
    // The runtime goes, and the connection with it.
    // SAFETY: plain syscalls, on the runtime and helper this test's link started.
    unsafe { libc::kill(-(before.pid as i32), libc::SIGKILL) };
    for pid in place.helpers() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let status = wait_status(&link, "connected again", |s| s.state == State::Connected || s.state == State::Failed);
    assert_eq!(status.runtime, None);
    assert!(place.runtime().is_none_or(|pid| pid == before.pid as i32), "nothing started a new one: {:?}", status.step);
    assert!(!pid_alive(before.pid as i32));
}

#[test]
fn an_idle_link_ends_and_leaves_the_runtime_running() {
    let place = Place::with("idle", &[("ENDEAVOR_LINK_IDLE_SECS", "2")]);
    let link = place.ensure();
    link.start(None).unwrap();
    let runtime = ready(&link).runtime.unwrap();
    // No request from here on.
    wait_for("the link to end", || !pid_alive(link.pid as i32));
    assert!(!place.record().exists(), "its record is removed");
    wait_for("its helper to go", || place.helpers().is_empty());
    assert!(pid_alive(runtime.pid as i32), "the runtime goes on");
    assert!(place.log().contains("the runtime keeps running"), "{}", place.log());

    // The next front starts a new link, which attaches to the same runtime.
    let again = place.ensure();
    assert_ne!(again.pid, link.pid);
    again.start(None).unwrap();
    let attached = ready(&again).runtime.unwrap();
    assert_eq!((attached.pid, attached.reattached), (runtime.pid, true));
}

#[test]
fn quitting_detaches_and_removes_the_record() {
    let place = Place::new("quit");
    let link = place.ensure();
    link.start(None).unwrap();
    let runtime = ready(&link).runtime.unwrap();
    link.quit().expect("quit");
    wait_for("the link to end", || !pid_alive(link.pid as i32));
    assert!(!place.record().exists());
    wait_for("its helper to go", || place.helpers().is_empty());
    assert!(pid_alive(runtime.pid as i32), "the runtime goes on");
    assert!(link.status().is_err(), "nothing answers on its port");
}

#[test]
fn a_stop_signal_detaches_and_removes_the_record() {
    let place = Place::new("signal");
    let link = place.ensure();
    link.start(None).unwrap();
    let runtime = ready(&link).runtime.unwrap();
    // SAFETY: plain syscall, on the link this test started.
    unsafe { libc::kill(link.pid as i32, libc::SIGTERM) };
    wait_for("the link to end", || !pid_alive(link.pid as i32));
    assert!(!place.record().exists());
    wait_for("its helper to go", || place.helpers().is_empty());
    assert!(pid_alive(runtime.pid as i32));
}

#[test]
fn the_control_port_wants_the_token_a_loopback_host_and_no_origin() {
    let place = Place::new("access");
    let link = place.ensure();
    let ask = |extra: &str| http(link.port, &format!("GET /link/status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{extra}Connection: close\r\n\r\n", link.port));
    let bearer = format!("Authorization: Bearer {}\r\n", link.token);
    assert_eq!(ask(&bearer).0, 200);
    assert_eq!(ask("").0, 401);
    assert_eq!(ask("Authorization: Bearer wrong\r\n").0, 401);
    assert_eq!(ask(&format!("{bearer}Origin: http://127.0.0.1:{}\r\n", link.port)).0, 403, "a page, even with the token");
    assert_eq!(ask(&format!("{bearer}Origin: https://example.com\r\n")).0, 403);
    let strange = http(link.port, &format!("GET /link/status HTTP/1.1\r\nHost: evil.example\r\n{bearer}\r\n"));
    assert_eq!(strange.0, 403, "a name that isn't loopback");
    let at = |method: &str, path: &str| http(link.port, &format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{bearer}Content-Length: 0\r\n\r\n")).0;
    assert_eq!((at("GET", "/link/nope"), at("GET", "/link/start"), at("POST", "/link/status")), (404, 405, 405));
    let bad = format!("POST /link/start HTTP/1.1\r\nHost: localhost\r\n{bearer}Content-Length: 12\r\n\r\n{{\"job\":\"no\"}}");
    assert_eq!(http(link.port, &bad).0, 400, "a job that isn't one");
    assert_eq!(link.status().unwrap().machine, place.id, "none of that ended the link");
}

#[test]
fn a_failed_connect_is_reported_once_and_tried_again_only_when_asked() {
    // Installing under a file can't work.
    let place = Place::with("failed", &[("ENDEAVOR_LINK_ROOT", "{dir}/blocker/root")]);
    std::fs::write(place.dir.join("blocker"), "").unwrap();
    let link = place.ensure();
    let status = wait_status(&link, "failed", |s| s.state == State::Failed);
    let error = status.error.expect("an error");
    assert!(error.contains("installing into") && error.contains("failed"), "{error}");
    std::thread::sleep(Duration::from_secs(3));
    let attempts = |log: &str| log.matches("The connection to lab failed").count();
    assert_eq!(attempts(&place.log()), 1, "no loop: {}", place.log());
    link.start(None).unwrap();
    wait_for("a second attempt", || attempts(&place.log()) == 2);
    wait_status(&link, "failed again", |s| s.state == State::Failed);
}

#[test]
fn a_link_for_a_machine_that_is_not_listed_says_so() {
    let place = Place::new("unlisted");
    let error = ensure_with(&place.spawn, "lab-other").expect_err("no such machine");
    assert!(error.contains("There is no machine lab-other") && error.contains("machines.json"), "{error}");
    let error = ensure_with(&place.spawn, "../escape").expect_err("not an id");
    assert!(error.contains("isn't a machine id"), "{error}");
}
