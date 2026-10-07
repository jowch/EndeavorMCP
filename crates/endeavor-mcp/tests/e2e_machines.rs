//! The machine tools of `endeavor mcp` over real ssh and real Julia: an agent's
//! harness (stdio) adds a machine, puts the session on it, works in a notebook
//! there, reads its files and runs a command on it, opens the notebook's page
//! through the front's port for the machine, and stops the runtime, until nothing is left
//! running. It's ignored by default and runs only when `ENDEAVOR_TEST_SSH_HOST`
//! names a host that this user can `ssh` to with a key (`localhost` is one):
//!
//!     ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines -- --ignored --nocapture
//!
//! Julia is `ENDEAVOR_E2E_JULIA` if set, else the app's own, else `julia` on the
//! PATH, and it has to be at the same path on the host. Its depot is
//! `e2e_client`'s, in `target/tmp/e2e-client/depot`, so run that test first or
//! expect several minutes. The front's state, config and cache folders, the
//! machines file, and the helper's install and state
//! folders are under `target/tmp/e2e-machines`. `HOME` stays the user's, where
//! `ssh` finds its keys, so `list_machines` reads the real `~/.ssh/config`; the
//! test doesn't look at what it lists. The front's own runtime (this computer's)
//! is pointed at a Julia that doesn't exist, so none is started here.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::front::Front;
use common::{find_julia, pid_alive, wait_for};
use endeavor_mcp::client::MachinesFile;
use serde_json::{Value, json};

/// A failed step leaves no Julia behind.
struct Ends {
    state: PathBuf,
}

impl Drop for Ends {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).filter(|&p| p > 1) {
            // SAFETY: plain syscall, on the runtime this test started: the core, Julia and its workers are one process group.
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        }
    }
}

/// The helpers (`endeavor connect`) on this computer that have `state` as their state folder.
fn helpers(state: &Path) -> Vec<u8> {
    Command::new("pgrep").arg("-f").arg("--").arg(format!("connect --state-dir {}", state.display())).output().unwrap().stdout
}

/// One GET to `port`: the status line, the head and the body.
fn get(port: u16, target: &str, headers: &str) -> (String, String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    write!(socket, "GET {target} HTTP/1.0\r\nHost: localhost:{port}\r\n{headers}\r\n").unwrap();
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply).unwrap();
    let reply = String::from_utf8_lossy(&reply).into_owned();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.lines().next().unwrap_or_default().to_owned(), head.to_owned(), body.to_owned())
}

#[test]
#[ignore = "needs ENDEAVOR_TEST_SSH_HOST and starts real Julia, several minutes the first time: ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines -- --ignored"]
fn the_machine_tools_over_real_ssh() {
    let Ok(host) = std::env::var("ENDEAVOR_TEST_SSH_HOST") else {
        eprintln!("SKIPPED: ENDEAVOR_TEST_SSH_HOST isn't set. Name a host this user can ssh to with a key, such as localhost.");
        return;
    };
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return;
    };
    let started = Instant::now();
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-machines");
    let depot = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-client/depot");
    drop(Ends { state: work.join("state") });
    let _ = std::fs::remove_dir_all(&work);
    for folder in ["root", "state", "notebooks", "project", "config", "state-home", "cache"] {
        std::fs::create_dir_all(work.join(folder)).unwrap();
    }
    std::fs::create_dir_all(&depot).unwrap();
    let canon = |p: &Path| p.canonicalize().unwrap();
    let (work, depot) = (canon(&work), canon(&depot));
    let (root, state, notebooks, project) = (work.join("root"), work.join("state"), work.join("notebooks"), work.join("project"));
    let depot_path = match &app {
        Some(app) => format!("{}:{}:", depot.display(), app.join("depot").display()),
        None => format!("{}:", depot.display()),
    };
    let _ends = Ends { state: state.clone() };
    eprintln!("host: {host}, julia: {}", julia.display());

    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command
        .args(["mcp", "--skills", "plugin", "--folder"])
        .arg(&project)
        .args(["--julia", "/nonexistent/julia", "--depot"])
        .arg(work.join("local-depot"))
        .arg("--state-dir")
        .arg(work.join("local-state"))
        .env("XDG_STATE_HOME", work.join("state-home"))
        .env("XDG_CONFIG_HOME", work.join("config"))
        .env("XDG_CACHE_HOME", work.join("cache"))
        .env("ENDEAVOR_TEST_ROOT", &root)
        .env("ENDEAVOR_TEST_STATE", &state)
        .env("ENDEAVOR_TEST_DEPOT", &depot_path)
        .env("ENDEAVOR_START_WAIT_SECS", "45")
        .current_dir(&project);
    let mut front = Front::spawn(command);
    front.initialize();

    let listed = front.ok("list_machines", json!({}));
    assert_eq!(listed["machines"], json!([]), "{listed}");
    assert!(listed["ssh_hosts_not_added"].is_array());
    assert!(helpers(&state).is_empty(), "listing connects to nothing");

    // The host may have Slurm, as a workstation or login node may; this test runs Julia there directly, not in a job.
    let looked = front.ok("add_machine", json!({ "host": host, "name": "e2e-machines", "julia": julia.display().to_string(), "slurm": false }));
    assert_eq!(looked["state"], "needs_install", "{looked}");
    assert!(!root.join(endeavor_mcp::embedded::BUILD_VERSION).exists(), "a look installs nothing");
    let added = front.ok("add_machine", json!({ "host": host, "name": "e2e-machines", "julia": julia.display().to_string(), "slurm": false, "install": true }));
    eprintln!("[{:?}] add_machine: {}", started.elapsed(), added["message"]);
    assert_eq!((added["state"].as_str(), added["saved"].clone()), (Some("connected"), json!(true)), "{added}");
    assert!(added["node"].as_str().is_some_and(|n| !n.is_empty()) && added["home"].as_str().is_some_and(|h| h.starts_with('/')), "{added}");
    assert_eq!((added["cluster"].clone(), added["runs_in"].clone()), (json!(false), json!("directly")), "{added}");
    let machines_file = MachinesFile::at(work.join("config/endeavor/machines.json"));
    let record = machines_file.find_by_name("e2e-machines").unwrap().expect("the machine is saved");
    assert!(record.cluster.is_none(), "saved as a plain server, whether or not Slurm is there: {added}");
    let installed = root.join(endeavor_mcp::embedded::BUILD_VERSION);
    assert!(installed.join("endeavor").is_file(), "the helper is installed in {}", installed.display());

    let folder = notebooks.display().to_string();
    let used = loop {
        let used = front.ok("use_machine", json!({ "machine": "e2e-machines", "folder": folder }));
        if used["ready"] == true {
            break used;
        }
        assert!(matches!(used["state"].as_str(), Some("starting" | "connecting")), "a plain server is started, never submitted for: {used}");
        assert!(started.elapsed() < Duration::from_secs(1500), "{used}");
        eprintln!("[{:?}] use_machine: {}", started.elapsed(), used["message"]);
    };
    eprintln!("[{:?}] use_machine ready on {}", started.elapsed(), used["node"]);
    let page = used["browser_url"].as_str().unwrap().to_owned();
    let (port, token) = page.strip_prefix("http://localhost:").unwrap().split_once("/?token=").map(|(p, t)| (p.parse::<u16>().unwrap(), t.to_owned())).unwrap();
    assert_eq!(used["folder"], folder);
    let remote_port = std::fs::read_to_string(state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["port"].as_u64()).expect("the runtime's port in its record");
    assert_eq!(used["remote_port"], remote_port, "{used}");
    assert_ne!(u64::from(port), remote_port, "the page's address is this computer's own port");
    let (ssh_host, ssh_port) = endeavor_mcp::client::Server::parse_target(&host).unwrap();
    let via = ssh_port.map(|p| format!(" -p {p}")).unwrap_or_default();
    assert!(used["message"].as_str().unwrap().contains(&format!("`ssh -L {remote_port}:127.0.0.1:{remote_port}{via} {ssh_host}`")), "{used}");

    let created = front.ok("new_notebook", json!({ "path": "analysis.jl" }));
    let notebook = created["notebook_id"].as_str().expect("a notebook").to_owned();
    assert_eq!(created["path"], notebooks.join("analysis.jl").display().to_string(), "the notebook is in the session's folder on the machine: {created}");
    assert_eq!(created["browser_url"], json!(format!("http://localhost:{port}/edit?id={notebook}&token={token}")), "{created}");
    let order = front.ok("get_cell_order", json!({ "notebook_id": notebook }));
    let last = order["cell_ids"].as_array().unwrap().last().unwrap().clone();
    let added = front.ok("add_cell", json!({ "notebook_id": notebook, "code": "x = 21 * 2", "after_cell_id": last }));
    let cell = added["cell_id"].as_str().unwrap().to_owned();
    front.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": cell, "wait_for_completion": true }));
    let read = front.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
    assert_eq!((&read["output"], &read["errored"]), (&json!("42"), &json!(false)), "{read}");
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!((status["machine"].as_str(), status["browser_url"].as_str()), (Some("e2e-machines"), Some(page.as_str())), "{status}");
    eprintln!("[{:?}] a cell ran on the machine", started.elapsed());

    // The server's files and shell, which the agent's own tools can't see.
    let folder_list = front.ok("list_folder", json!({ "path": folder }));
    assert!(folder_list.to_string().contains("analysis.jl"), "{folder_list}");
    let file = front.ok("read_file", json!({ "path": notebooks.join("analysis.jl").display().to_string() }));
    assert!(file.to_string().contains("x = 21 * 2"), "{file}");
    let shell = front.ok("run_shell", json!({ "command": "pwd; hostname" }));
    assert!(shell.to_string().contains(&folder), "the session's folder: {shell}");

    // The page, as a browser reaches it through the front's port for the machine.
    let (line, head, _) = get(port, &format!("/edit?id={notebook}&token={token}"), "");
    assert_eq!(line, "HTTP/1.1 303 See Other", "{head}");
    let cookie = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}")).split(';').next().unwrap().to_owned();
    let (line, head, body) = get(port, &format!("/edit?id={notebook}"), &format!("Cookie: {cookie}\r\nSec-Fetch-Site: none\r\n"));
    assert_eq!(line, "HTTP/1.1 200 OK", "{head}");
    assert!(body.contains("Pluto"), "{}", &body[..body.len().min(300)]);
    eprintln!("[{:?}] the page answers", started.elapsed());

    // The connection drops, as it does when the network fails: the front makes it again, the address the
    // user has open stays the same, and it attaches to the runtime that kept running.
    let runtime_pid = std::fs::read_to_string(state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).expect("the runtime's record") as i32;
    let pids = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).split_whitespace().filter_map(|p| p.parse::<i32>().ok()).collect::<Vec<_>>();
    let lost = pids(helpers(&state));
    assert!(!lost.is_empty(), "the front is connected");
    for pid in &lost {
        // SAFETY: plain syscall, on a helper (or the ssh to it) of this test's own front, found by its state folder.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    wait_for("the connection to be made again", || {
        let now = pids(helpers(&state));
        !now.is_empty() && now.iter().all(|pid| !lost.contains(pid))
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let again = loop {
        let (failed, again) = front.call("pluto_session_status", json!({}));
        if !failed && again.get("browser_url").is_some() {
            break again;
        }
        assert!(Instant::now() < deadline, "{again}\n{:?}", front.said());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(again["browser_url"].as_str(), Some(page.as_str()), "the address the user has open is the same: {again}");
    assert!(pid_alive(runtime_pid), "the runtime kept running");
    let read = front.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
    assert_eq!((&read["output"], &read["errored"]), (&json!("42"), &json!(false)), "the notebook is still there after the connection came back: {read}");
    eprintln!("[{:?}] the connection was made again", started.elapsed());

    let stopped = front.ok("stop_machine", json!({ "machine": "e2e-machines" }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the runtime to end", || !pid_alive(runtime_pid));
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && said.as_str().is_some_and(|t| t.contains("use_machine")), "{said}");
    front.finish();

    // Nothing is left: the front's end took its connection with it.
    wait_for("the helper to go", || helpers(&state).is_empty());
    assert!(!state.join("runtime.json").exists());
    eprintln!("[{:?}] stopped; nothing left", started.elapsed());
}
