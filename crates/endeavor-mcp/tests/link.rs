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
use endeavor_mcp::client::Server;
use endeavor_mcp::link::{CALL_WAIT, Link, Spawn, State, Status, ensure_with};
use serde_json::{Value, json};

/// One machine called `lab-NAME` whose helper, runtime and link all live in a folder of the test's own.
struct Place {
    dir: PathBuf,
    id: String,
    /// The record the link is started with. No machines file is written: the link needs none.
    server: Server,
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
        let server = Server { id: id.clone(), name: "lab".into(), ssh_host: "lab".into(), julia: Some(julia.display().to_string()), ..Default::default() };
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
        Place { dir, id, server, state, spawn, _bridge: bridge }
    }

    /// The link, with the user's agreement to install the helper given.
    fn ensure(&self) -> Link {
        let link = self.look();
        link.install(CALL_WAIT).expect("install");
        link
    }

    /// The link as `ensure` starts it: it looks and installs nothing.
    fn look(&self) -> Link {
        ensure_with(&self.spawn, &self.server).expect("a link")
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
        let status = link.status(CALL_WAIT).expect("status");
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
        let (spawn, server) = (&place.spawn, &place.server);
        let starts: Vec<_> = (0..4).map(|_| scope.spawn(|| ensure_with(spawn, server).expect("a link"))).collect();
        starts.into_iter().map(|s| s.join().unwrap()).collect()
    });
    assert!(links.iter().all(|l| l == &links[0]), "{links:?}");
    let link = links[0].clone();
    link.install(CALL_WAIT).expect("install");
    assert!(pid_alive(link.pid as i32));
    assert_eq!(place.ensure(), link, "a later front reuses it");
    assert_eq!(pids("link --machine lab-ready").len(), 1, "one process");
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(place.record()).unwrap()).unwrap();
    assert_eq!((on_disk["pid"].as_u64(), on_disk["port"].as_u64(), on_disk["token"].as_str()), (Some(link.pid as u64), Some(link.port as u64), Some(link.token.as_str())));
    assert_eq!(on_disk["build"], endeavor_mcp::embedded::BUILD_VERSION);
    assert_eq!((on_disk["protocol"].as_u64(), link.protocol), (Some(endeavor_mcp::link::PROTOCOL as u64), endeavor_mcp::link::PROTOCOL));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(place.record()).unwrap().permissions().mode() & 0o777, 0o600);

    // Connected, with what the helper said, and no runtime until it is asked for.
    let status = wait_status(&link, "connected", |s| s.state == State::Connected);
    let hello = status.hello.clone().expect("the helper's hello");
    assert!(!hello.node.is_empty() && hello.uploads);
    let uname = String::from_utf8(std::process::Command::new("uname").arg("-s").output().unwrap().stdout).unwrap();
    assert_eq!((hello.helper_installed, hello.os.as_deref()), (Some(true), Some(uname.trim())));
    assert_eq!((status.machine.as_str(), status.name.as_str(), status.runtime.clone()), (place.id.as_str(), "lab", None));
    assert!(place.runtime().is_none());

    let asked = link.start(None, false, CALL_WAIT).expect("start");
    assert!(matches!(asked.state, State::Connected | State::Starting), "answered at once: {asked:?}");
    let status = ready(&link);
    let runtime = status.runtime.clone().expect("the runtime");
    assert_eq!(runtime.token, TOKEN);
    assert!(!runtime.reattached && runtime.pid as i32 == place.runtime().unwrap());
    assert_eq!(runtime.page_url, format!("http://127.0.0.1:{}/?token={TOKEN}", runtime.port));
    assert_eq!(runtime.mcp_url, format!("http://127.0.0.1:{}/mcp", runtime.port));
    assert_ne!(runtime.port, link.port, "the control port isn't the listener's");
    assert_eq!(status.hello.unwrap().found.first().map(|f| (f.name.clone(), f.version.clone())), Some(("Julia".into(), "1.12.0".into())));

    // A start with the runtime attached is not an error and changes nothing.
    let again = link.start(None, false, CALL_WAIT).expect("start again");
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
    link.start(None, false, CALL_WAIT).unwrap();
    let first = ready(&link).runtime.unwrap();
    link.stop().expect("stop");
    let status = link.status(CALL_WAIT).unwrap();
    assert_eq!((status.state, status.runtime), (State::Connected, None));
    assert!(!pid_alive(first.pid as i32), "the runtime is gone");
    assert!(!place.helpers().is_empty(), "the link stays connected");
    // The listener says what to do, in the link's words.
    let said = tool(first.port, &first.token, "list_notebooks", None);
    assert!(said.as_str().is_some_and(|text| text.contains("Call use_machine")), "{said}");

    link.start(None, false, CALL_WAIT).unwrap();
    let second = ready(&link).runtime.unwrap();
    assert_eq!(second.port, first.port, "the same listener");
    assert!(!second.reattached && second.pid != first.pid);
    assert!(tool(second.port, &second.token, "pluto_session_status", Some(second.port)).get("browser_url").is_some());
    // Nothing runs, so a stop says why it can't.
    link.stop().unwrap();
    assert!(link.stop().is_ok(), "the helper has nothing to stop and says so");
}

#[test]
fn an_attach_starts_nothing_when_no_runtime_is_there_and_takes_the_one_that_is() {
    let place = Place::new("attach");
    let link = place.ensure();
    // Asked at once, while the link may still be connecting: it asks the helper once connected.
    link.attach(false, CALL_WAIT).expect("attach");
    let status = wait_status(&link, "nothing running", |s| s.nothing_running);
    assert_eq!((status.state, status.runtime.is_none(), status.error.clone()), (State::Connected, true, None), "{status:?}");
    assert!(place.runtime().is_none() && !place.state.join("julia.args").exists(), "no Julia was started");
    assert!(status.step.unwrap().contains("No runtime is running"));

    // A start after it is a start, and the flag clears.
    let asked = link.start(None, false, CALL_WAIT).unwrap();
    assert!(!asked.nothing_running);
    let runtime = ready(&link).runtime.unwrap();
    assert!(!runtime.reattached);

    // Another link attaches to what runs, and is not told that nothing does.
    link.quit().unwrap();
    wait_for("the link to end", || !pid_alive(link.pid as i32));
    let second = place.ensure();
    assert_ne!(second.pid, link.pid);
    second.attach(false, CALL_WAIT).unwrap();
    let status = ready(&second);
    let attached = status.runtime.unwrap();
    assert!(attached.reattached && attached.pid == runtime.pid, "{attached:?}");
    assert!(!status.nothing_running);
    // With a runtime attached, an attach changes nothing.
    assert_eq!(second.attach(false, CALL_WAIT).unwrap().state, State::Ready);
}

#[test]
fn the_connection_comes_back_on_the_same_port() {
    let place = Place::new("reconnect");
    let link = place.ensure();
    link.start(None, false, CALL_WAIT).unwrap();
    let before = ready(&link).runtime.unwrap();
    let config = place.dir.join("config/endeavor/machines.json");
    assert!(!config.exists(), "the link ran with no machines file");
    // A record of the same id in the file that would not connect: a reconnect keeps the record the link was started with.
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, json!({ "schema": 1, "machines": [{ "id": place.id, "name": "lab", "ssh_host": "elsewhere", "julia": "/no/such/julia" }] }).to_string()).unwrap();
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
    link.start(None, false, CALL_WAIT).unwrap();
    let before = ready(&link).runtime.unwrap();
    // The runtime goes, and the connection with it.
    // SAFETY: plain syscalls, on the runtime and helper this test's link started.
    unsafe { libc::kill(-(before.pid as i32), libc::SIGKILL) };
    for pid in place.helpers() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let status = wait_status(&link, "the end told", |s| s.state == State::Failed);
    assert_eq!(status.runtime, None);
    assert!(status.error.as_deref().is_some_and(|e| e.contains("Julia on lab")), "{status:?}");
    assert!(place.runtime().is_none_or(|pid| pid == before.pid as i32), "nothing started a new one: {:?}", status.step);
    assert!(!pid_alive(before.pid as i32));
    // The listener no longer says that the connection comes back by itself.
    let said = tool(before.port, &before.token, "list_notebooks", None);
    assert!(said.as_str().is_some_and(|text| text.contains("Call use_machine") && !text.contains("by itself")), "{said}");
    // And a start asked for now works.
    link.start(None, false, CALL_WAIT).unwrap();
    assert_ne!(ready(&link).runtime.unwrap().pid, before.pid);
}

#[test]
fn an_idle_link_ends_and_leaves_the_runtime_running() {
    let place = Place::with("idle", &[("ENDEAVOR_LINK_IDLE_SECS", "2")]);
    let link = place.ensure();
    link.start(None, false, CALL_WAIT).unwrap();
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
    again.start(None, false, CALL_WAIT).unwrap();
    let attached = ready(&again).runtime.unwrap();
    assert_eq!((attached.pid, attached.reattached), (runtime.pid, true));
}

#[test]
fn quitting_detaches_and_removes_the_record() {
    let place = Place::new("quit");
    let link = place.ensure();
    link.start(None, false, CALL_WAIT).unwrap();
    let runtime = ready(&link).runtime.unwrap();
    link.quit().expect("quit");
    wait_for("the link to end", || !pid_alive(link.pid as i32));
    assert!(!place.record().exists());
    wait_for("its helper to go", || place.helpers().is_empty());
    assert!(pid_alive(runtime.pid as i32), "the runtime goes on");
    assert!(link.status(CALL_WAIT).is_err(), "nothing answers on its port");
}

#[test]
fn a_stop_signal_detaches_and_removes_the_record() {
    let place = Place::new("signal");
    let link = place.ensure();
    link.start(None, false, CALL_WAIT).unwrap();
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
    assert_eq!(link.status(CALL_WAIT).unwrap().machine, place.id, "none of that ended the link");
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
    link.start(None, false, CALL_WAIT).unwrap();
    wait_for("a second attempt", || attempts(&place.log()) == 2);
    wait_status(&link, "failed again", |s| s.state == State::Failed);
}

#[test]
fn a_record_whose_id_is_not_an_id_is_refused() {
    let place = Place::new("bad-id");
    let error = ensure_with(&place.spawn, &Server { id: "../escape".into(), ..place.server.clone() }).expect_err("not an id");
    assert!(error.contains("isn't a machine id"), "{error}");
}

#[test]
fn a_link_started_by_hand_without_a_record_says_so() {
    let place = Place::new("no-record");
    let folder = place.dir.join("state-home/endeavor/links").join(&place.id);
    std::fs::create_dir_all(&folder).unwrap();
    let env = place.spawn.env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
    let out = Command::new(&place.spawn.exe).args(["link", "--machine", &place.id]).envs(env).output().unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("server.json") && said.contains("front writes"), "{said}");
}

#[test]
fn a_connection_lost_while_the_runtime_starts_is_resumed_after_the_reconnect() {
    // Connecting waits while `gate` exists, so that the runtime is up before the link is back.
    let place = Place::with("lost-starting", &[("ENDEAVOR_LINK_ASK", "while [ -e {dir}/gate ]; do sleep 0.1; done")]);
    let (hold, gate) = (place.state.join("hold"), place.dir.join("gate"));
    std::fs::write(&hold, "").unwrap();
    let link = place.ensure();
    link.start(None, false, CALL_WAIT).unwrap();
    wait_status(&link, "starting", |s| s.state == State::Starting);
    wait_for("Julia to be asked for", || place.state.join("julia.args").exists());
    std::fs::write(&gate, "").unwrap();
    let helpers = place.helpers();
    assert!(!helpers.is_empty());
    for pid in &helpers {
        // SAFETY: plain syscall, on the helper this test's link started.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    wait_status(&link, "the connection lost", |s| s.state == State::Connecting);
    std::fs::remove_file(&hold).unwrap();
    wait_for("the runtime to come up without the link", || place.runtime().is_some());
    std::fs::remove_file(&gate).unwrap();
    let runtime = ready(&link).runtime.expect("the runtime");
    assert_eq!((runtime.pid as i32, runtime.reattached), (place.runtime().unwrap(), true));
}

#[test]
fn a_stop_during_a_start_is_no_failure() {
    let place = Place::new("stop-starting");
    let hold = place.state.join("hold");
    std::fs::write(&hold, "").unwrap();
    let link = place.ensure();
    link.start(None, false, CALL_WAIT).unwrap();
    wait_status(&link, "starting", |s| s.state == State::Starting);
    wait_for("Julia to be asked for", || place.state.join("julia.args").exists());
    link.stop().expect("stop");
    std::fs::remove_file(&hold).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let status = link.status(CALL_WAIT).unwrap();
    assert_eq!((status.state, status.error, status.runtime), (State::Connected, None, None));
}

#[test]
fn a_front_that_asks_right_after_a_quit_gets_a_new_link() {
    let place = Place::new("quit-ensure");
    let link = place.ensure();
    link.quit().expect("quit");
    let again = place.ensure();
    assert_ne!(again.pid, link.pid);
    assert!(again.status(CALL_WAIT).is_ok());
    wait_for("the first link to end", || !pid_alive(link.pid as i32));
    assert_eq!(place.ensure(), again, "the record is the new link's");
}

#[test]
fn a_link_that_lives_and_does_not_answer_is_not_replaced() {
    let place = Place::new("silent");
    // A port that nothing listens on, and a process that lives: this test.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    std::fs::create_dir_all(place.record().parent().unwrap()).unwrap();
    let record = json!({ "machine": place.id, "pid": std::process::id(), "port": port, "token": "t", "build": "x" });
    std::fs::write(place.record(), record.to_string()).unwrap();
    let error = ensure_with(&place.spawn, &place.server).expect_err("nothing answers");
    // Removed before anything can fail: cleaning up ends the pid in the record.
    let kept = place.record().exists();
    std::fs::remove_file(place.record()).unwrap();
    assert!(error.contains(&format!("(pid {})", std::process::id())) && error.contains("isn't answering") && error.contains("Try again"), "{error}");
    assert!(kept, "the record stays");
    assert!(pids("link --machine lab-silent").is_empty(), "no second link");

    // A record of a process that is gone is replaced.
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    std::fs::write(place.record(), json!({ "machine": place.id, "pid": pid, "port": port, "token": "t", "build": "x" }).to_string()).unwrap();
    let link = place.ensure();
    assert_ne!(link.pid, pid);
}

#[test]
fn a_link_that_never_answers_is_ended_by_the_front_that_started_it() {
    let place = Place::new("never");
    let script = place.dir.join("never-answers");
    std::fs::write(&script, format!("#!/bin/sh\necho $$ > {}/never.pid\nexec sleep 600\n", place.dir.display())).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let spawn = Spawn { exe: script, env: place.spawn.env.clone() };
    let error = ensure_with(&spawn, &place.server).expect_err("no answer");
    assert!(error.contains("didn't answer"), "{error}");
    let pid: i32 = std::fs::read_to_string(place.dir.join("never.pid")).unwrap().trim().parse().unwrap();
    wait_for("the child to be ended", || !pid_alive(pid));
}

#[test]
fn a_start_asked_for_while_connecting_does_not_make_another_attempt() {
    // The sign-in takes 2 s and then fails.
    let place = Place::with("kicks", &[("ENDEAVOR_LINK_ASK", "echo x >> {dir}/attempts; sleep 2; exit 1")]);
    let link = place.ensure();
    for _ in 0..5 {
        link.start(None, false, CALL_WAIT).unwrap();
    }
    wait_status(&link, "failed", |s| s.state == State::Failed);
    std::thread::sleep(Duration::from_secs(2));
    let attempts = || std::fs::read_to_string(place.dir.join("attempts")).unwrap_or_default().lines().count();
    assert_eq!(attempts(), 1, "{}", place.log());
    link.start(None, false, CALL_WAIT).unwrap();
    wait_for("a second attempt", || attempts() == 2);
    wait_status(&link, "failed again", |s| s.state == State::Failed);
    assert_eq!(attempts(), 2);
}

#[test]
fn a_connection_that_cannot_come_back_tells_the_listener() {
    // The sign-in is refused once `refuse` exists: that is not a failure that trying again could fix.
    let place = Place::with("given-up", &[("ENDEAVOR_LINK_ASK", "[ ! -e {dir}/refuse ] || { echo 'Permission denied (publickey)' >&2; exit 255; }")]);
    let link = place.ensure();
    link.start(None, false, CALL_WAIT).unwrap();
    let runtime = ready(&link).runtime.unwrap();
    std::fs::write(place.dir.join("refuse"), "").unwrap();
    for pid in place.helpers() {
        // SAFETY: plain syscall, on the helper this test's link started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let status = wait_status(&link, "failed", |s| s.state == State::Failed);
    assert!(status.error.is_some_and(|e| e.contains("refused the sign-in")));
    let said = tool(runtime.port, &runtime.token, "list_notebooks", None);
    assert!(said.as_str().is_some_and(|text| text.contains("Call use_machine") && !text.contains("by itself")), "{said}");
    assert!(link.status(CALL_WAIT).is_ok(), "the link stays alive and answers");
}

#[test]
fn an_idle_time_that_is_no_time_is_ignored() {
    for (name, value) in [("idle-negative", "-5"), ("idle-nan", "NaN"), ("idle-inf", "inf"), ("idle-zero", "0"), ("idle-huge", "1e300")] {
        let place = Place::with(name, &[("ENDEAVOR_LINK_IDLE_SECS", value)]);
        let link = place.ensure();
        std::thread::sleep(Duration::from_millis(600));
        assert!(pid_alive(link.pid as i32) && link.status(CALL_WAIT).is_ok(), "{value}: {}", place.log());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_link_works_in_a_folder_of_its_own() {
    let place = Place::new("cwd");
    let link = place.ensure();
    let cwd = std::fs::read_link(format!("/proc/{}/cwd", link.pid)).unwrap();
    assert_eq!(cwd, place.record().parent().unwrap().canonicalize().unwrap());
}

#[test]
fn a_machine_without_the_helper_waits_for_the_user_and_installs_once_told() {
    let place = Place::new("needs-install");
    let root = place.dir.join("root");
    let link = place.look();
    let status = wait_status(&link, "needs_install", |s| s.state == State::NeedsInstall);
    let needs = status.needs_install.clone().expect("what it needs");
    assert_eq!(needs.items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(), [wire::KIND_HELPER], "the helper is the one item");
    assert_eq!(needs.items[0].place.as_deref(), Some(root.join(endeavor_mcp::embedded::BUILD_VERSION).to_str().unwrap()));
    let helper = needs.helper.expect("the helper's details");
    assert_eq!(Path::new(&helper.folder), root.join(endeavor_mcp::embedded::BUILD_VERSION));
    assert!(helper.bytes.is_some_and(|bytes| bytes > 1000) && !helper.update && helper.running.is_none(), "{helper:?}");
    assert_eq!(status.error, None, "it isn't a failure");
    assert!(status.hello.is_some_and(|h| h.os.is_some()), "what it found is kept");

    // A start without the agreement asks again and finds the same, and nothing is written there.
    link.start(None, false, CALL_WAIT).unwrap();
    wait_for("the second look", || link.status(CALL_WAIT).is_ok_and(|s| s.state == State::NeedsInstall && s.step.as_deref().is_some_and(|step| step.contains("isn't installed"))));
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(link.status(CALL_WAIT).unwrap().state, State::NeedsInstall, "not tried in a loop");
    assert!(!root.exists() && place.helpers().is_empty(), "nothing was installed");
    assert_eq!(place.log().matches("waiting for the user's agreement").count(), 2, "one look for each request: {}", place.log());

    // The agreement in the start installs and starts.
    link.start(None, true, Duration::from_secs(5)).unwrap();
    let status = ready(&link);
    assert_eq!(status.needs_install, None);
    assert_eq!(status.hello.unwrap().helper_installed, Some(true));
    assert!(root.join(endeavor_mcp::embedded::BUILD_VERSION).join("endeavor").exists());

    // The connection drops and the helper is gone from the machine: the reconnect installs it again, with no new question.
    let before = ready(&link).runtime.unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    for pid in place.helpers() {
        // SAFETY: plain syscall, on the helper this test's link started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let after = ready(&link).runtime.unwrap();
    assert_eq!(after.pid, before.pid);
    wait_for("the helper again", || !place.helpers().is_empty());
    assert!(root.join(endeavor_mcp::embedded::BUILD_VERSION).join("endeavor").exists(), "installed again by the reconnect");
    assert_ne!(link.status(CALL_WAIT).unwrap().state, State::NeedsInstall);
    link.stop().unwrap();

    // A new link process asks nothing: the helper is there.
    link.quit().unwrap();
    wait_for("the first link to end", || !pid_alive(link.pid as i32));
    let fresh = place.look();
    let status = wait_status(&fresh, "connected", |s| s.state == State::Connected);
    assert_eq!(status.hello.unwrap().helper_installed, Some(false));
    assert_eq!(status.needs_install, None);
}

#[test]
fn the_agreement_without_a_start_installs_the_helper_and_starts_nothing() {
    let place = Place::new("install-only");
    let link = place.look();
    wait_status(&link, "needs_install", |s| s.state == State::NeedsInstall);
    link.start(None, false, CALL_WAIT).unwrap();
    wait_status(&link, "needs_install", |s| s.state == State::NeedsInstall);
    // The wish for a runtime isn't kept for the agreement that comes later.
    link.install(CALL_WAIT).unwrap();
    let status = wait_status(&link, "connected", |s| s.state == State::Connected);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!((status.runtime, link.status(CALL_WAIT).unwrap().state), (None, State::Connected));
    assert!(place.runtime().is_none() && !place.state.join("julia.args").exists(), "no Julia was started");
}

#[test]
fn an_agreement_that_comes_while_connecting_is_not_lost() {
    let place = Place::with("install-racing", &[("ENDEAVOR_LINK_ASK", "sleep 1")]);
    let link = place.look();
    link.install(CALL_WAIT).unwrap();
    let status = wait_status(&link, "connected", |s| s.state == State::Connected);
    assert_eq!(status.hello.unwrap().helper_installed, Some(true));
}

#[test]
fn a_start_body_with_install_goes_through() {
    let place = Place::new("install-field");
    let link = place.look();
    let call = |body: &str| {
        let request = format!("POST /link/start HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}", link.port, link.token, body.len());
        http(link.port, &request).0
    };
    assert_eq!(call(r#"{"job":null,"install":true}"#), 200);
    ready(&link);
}

#[test]
fn an_agreement_given_with_a_connection_up_is_not_kept_for_the_helper() {
    let place = Place::new("agreement-not-kept");
    let root = place.dir.join("root");
    common::install_helper(&root);
    let link = place.look();
    wait_status(&link, "connected", |s| s.state == State::Connected);
    // With the helper connected, `install` can only be for what the start needs (Julia).
    link.start(None, true, Duration::from_secs(5)).unwrap();
    ready(&link);
    // The helper goes from the machine, and the connection with it: that reconnect asks.
    std::fs::remove_dir_all(&root).unwrap();
    for pid in place.helpers() {
        // SAFETY: plain syscall, on the helper this test's link started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let status = wait_status(&link, "needs_install", |s| s.state == State::NeedsInstall);
    assert_eq!(status.needs_install.map(|n| n.needs_helper()), Some(true));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!root.exists(), "nothing was installed without a question");
    // The agreement to the helper does it, and `stop` ends the runtime that was left.
    link.install(CALL_WAIT).unwrap();
    wait_status(&link, "connected", |s| s.state == State::Connected);
    link.stop().unwrap();
}
