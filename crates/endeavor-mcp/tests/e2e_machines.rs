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

fn helpers(state: &Path) -> Vec<u8> {
    Command::new("pgrep").arg("-f").arg("--").arg(format!("connect --state-dir {}", state.display())).output().unwrap().stdout
}

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

struct Connected {
    front: Front,
    started: Instant,
    work: PathBuf,
    state: PathBuf,
    notebooks: PathBuf,
    page: String,
    port: u16,
    _ends: Ends,
}

fn connected(dir: &str, name: &str) -> Option<Connected> {
    common::require_debug_build();
    let Ok(host) = std::env::var("ENDEAVOR_TEST_SSH_HOST") else {
        eprintln!("SKIPPED: ENDEAVOR_TEST_SSH_HOST isn't set. Name a host this user can ssh to with a key, such as localhost.");
        return None;
    };
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return None;
    };
    let started = Instant::now();
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(dir);
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
    let ends = Ends { state: state.clone() };
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
        .env("ENDEAVOR_TEST_BROWSER", work.join("opened.txt"))
        .env("ENDEAVOR_TEST_DEPOT", &depot_path)
        .env("ENDEAVOR_START_WAIT_SECS", "45")
        .current_dir(&project);
    let mut front = Front::spawn(command);
    front.initialize();

    let listed = front.ok("list_machines", json!({}));
    assert_eq!(listed["machines"], json!([]), "{listed}");
    assert!(listed["ssh_hosts_not_added"].is_array());
    assert!(helpers(&state).is_empty(), "listing connects to nothing");

    let looked = front.ok("add_machine", json!({ "host": host, "name": name, "julia": julia.display().to_string(), "slurm": false }));
    assert_eq!(looked["state"], "needs_install", "{looked}");
    assert!(!root.join(endeavor_mcp::embedded::BUILD_VERSION).exists(), "a look installs nothing");
    let added = front.ok("add_machine", json!({ "host": host, "name": name, "julia": julia.display().to_string(), "slurm": false, "install": true }));
    eprintln!("[{:?}] add_machine: {}", started.elapsed(), added["message"]);
    assert_eq!((added["state"].as_str(), added["saved"].clone()), (Some("connected"), json!(true)), "{added}");
    assert!(added["node"].as_str().is_some_and(|n| !n.is_empty()) && added["home"].as_str().is_some_and(|h| h.starts_with('/')), "{added}");
    assert_eq!((added["cluster"].clone(), added["runs_in"].clone()), (json!(false), json!("directly")), "{added}");
    let machines_file = MachinesFile::at(work.join("config/endeavor/machines.json"));
    let record = machines_file.find_by_name(name).unwrap().expect("the machine is saved");
    assert!(record.cluster.is_none(), "saved as a plain server, whether or not Slurm is there: {added}");
    let installed = root.join(endeavor_mcp::embedded::BUILD_VERSION);
    assert!(installed.join("endeavor").is_file(), "the helper is installed in {}", installed.display());

    let folder = notebooks.display().to_string();
    let used = loop {
        let used = front.ok("use_machine", json!({ "machine": name, "folder": folder }));
        if used["ready"] == true {
            break used;
        }
        assert!(matches!(used["state"].as_str(), Some("starting" | "connecting")), "a plain server is started, never submitted for: {used}");
        assert!(started.elapsed() < Duration::from_secs(1500), "{used}");
        eprintln!("[{:?}] use_machine: {}", started.elapsed(), used["message"]);
    };
    eprintln!("[{:?}] use_machine ready on {}", started.elapsed(), used["node"]);
    let page = used["browser_url"].as_str().unwrap().to_owned();
    let port: u16 = page.strip_prefix("http://localhost:").and_then(|rest| rest.strip_suffix('/')).unwrap_or_else(|| panic!("a page without the token: {page}")).parse().unwrap();
    assert_eq!(used["folder"], folder);
    let remote_port = std::fs::read_to_string(state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["port"].as_u64()).expect("the runtime's port in its record");
    assert_eq!(used["remote_port"], remote_port, "{used}");
    assert_ne!(u64::from(port), remote_port, "the page's address is this computer's own port");
    let (ssh_host, ssh_port) = endeavor_mcp::client::Server::parse_target(&host).unwrap();
    let via = ssh_port.map(|p| format!(" -p {p}")).unwrap_or_default();
    assert!(used["message"].as_str().unwrap().contains(&format!("`ssh -L {remote_port}:127.0.0.1:{remote_port}{via} {ssh_host}`")), "{used}");

    Some(Connected { front, started, work, state, notebooks, page, port, _ends: ends })
}

#[test]
#[ignore = "needs ENDEAVOR_TEST_SSH_HOST and starts real Julia, several minutes the first time: ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines -- --ignored"]
fn the_machine_tools_over_real_ssh() {
    let Some(Connected { mut front, started, work, state, notebooks, page, port, _ends }) = connected("e2e-machines", "e2e-machines") else { return };
    let folder = notebooks.display().to_string();
    let created = loop {
        let created = front.ok("new_notebook", json!({ "path": "analysis.jl" }));
        if created["error"] != "julia_starting" {
            break created;
        }
        assert!(started.elapsed() < Duration::from_secs(1500), "{created}");
        eprintln!("[{:?}] new_notebook: {}", started.elapsed(), created["message"]);
    };
    let notebook = created["notebook_id"].as_str().unwrap_or_else(|| panic!("a notebook: {created}")).to_owned();
    assert_eq!(created["path"], notebooks.join("analysis.jl").display().to_string(), "the notebook is in the session's folder on the machine: {created}");
    assert_eq!((&created["browser_url"], &created["opened_in_browser"]), (&json!(format!("http://localhost:{port}/edit?id={notebook}")), &json!(true)), "{created}");
    let opened = std::fs::read_to_string(work.join("opened.txt")).unwrap();
    let token = opened.lines().last().and_then(|line| line.strip_prefix(&format!("http://localhost:{port}/edit?id={notebook}&token="))).unwrap_or_else(|| panic!("the browser got the link with the token: {opened}")).to_owned();
    let order = front.ok("get_cell_order", json!({ "notebook_id": notebook }));
    let last = order["cell_ids"].as_array().unwrap().last().unwrap().clone();
    let added = front.ok("add_cell", json!({ "notebook_id": notebook, "code": "x = 21 * 2", "after_cell_id": last }));
    let cell = added["cell_id"].as_str().unwrap().to_owned();
    front.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": cell, "wait_for_completion": true }));
    let read = front.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
    assert_eq!((&read["output"], &read["errored"]), (&json!("42"), &json!(false)), "{read}");
    let status = front.ok("session_status", json!({}));
    assert_eq!((status["machine"].as_str(), status["browser_url"].as_str()), (Some("e2e-machines"), Some(page.as_str())), "{status}");
    eprintln!("[{:?}] a cell ran on the machine", started.elapsed());

    let folder_list = front.ok("list_folder", json!({ "path": folder }));
    assert!(folder_list.to_string().contains("analysis.jl"), "{folder_list}");
    let file = front.ok("read_file", json!({ "path": notebooks.join("analysis.jl").display().to_string() }));
    assert!(file.to_string().contains("x = 21 * 2"), "{file}");
    let shell = front.ok("run_shell", json!({ "command": "pwd; hostname" }));
    assert!(shell.to_string().contains(&folder), "the session's folder: {shell}");

    let (line, head, _) = get(port, &format!("/edit?id={notebook}&token={token}"), "");
    assert_eq!(line, "HTTP/1.1 303 See Other", "{head}");
    let cookie = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}")).split(';').next().unwrap().to_owned();
    let (line, head, body) = get(port, &format!("/edit?id={notebook}"), &format!("Cookie: {cookie}\r\nSec-Fetch-Site: none\r\n"));
    assert_eq!(line, "HTTP/1.1 200 OK", "{head}");
    assert!(body.contains("Pluto"), "{}", &body[..body.len().min(300)]);
    eprintln!("[{:?}] the page answers", started.elapsed());

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
        let (failed, again) = front.call("session_status", json!({}));
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

    wait_for("the helper to go", || helpers(&state).is_empty());
    assert!(!state.join("runtime.json").exists());
    eprintln!("[{:?}] stopped; nothing left", started.elapsed());
}

const GROWTH: &str = r#"# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "marimo==0.25.1",
# ]
# ///

import marimo

__generated_with = "0.25.1"
app = marimo.App()


@app.cell
def _():
    x = 41
    return (x,)


@app.cell
def _(x):
    y = x + 1
    y
    return


if __name__ == "__main__":
    app.run()
"#;

#[test]
#[ignore = "needs ENDEAVOR_TEST_SSH_HOST, starts real Julia and marimo and may download uv: ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines -- --ignored --test-threads 1"]
fn a_python_notebook_runs_on_a_machine() {
    let Some(Connected { mut front, started, work, state: _, notebooks, page: _, port, _ends }) = connected("e2e-machines-python", "e2e-python") else { return };
    std::fs::write(notebooks.join("growth.py"), GROWTH).unwrap();
    let began = Instant::now();
    let opened = loop {
        let (failed, said) = front.call("open_notebook", json!({ "path": "growth.py" }));
        let said: Value = said.as_str().and_then(|text| serde_json::from_str(text).ok()).unwrap_or(said);
        match said["error"].as_str() {
            Some("python_installing") => {}
            Some("python_not_found") => {
                let offer = said["message"].as_str().unwrap_or_default();
                assert!(offer.contains("`use_machine` with `machine` \"e2e-python\" and `install: true`"), "the offer names this machine: {offer}");
                eprintln!("[{:?}] no uv on the machine; saying yes to Endeavor's own", started.elapsed());
                let folder = notebooks.display().to_string();
                let again = front.ok("use_machine", json!({ "machine": "e2e-python", "folder": folder, "install": true }));
                assert!(!again["message"].as_str().unwrap_or_default().contains("Couldn't pass"), "{again}");
            }
            _ if !failed => break said,
            _ => panic!("{said}"),
        }
        assert!(began.elapsed() < Duration::from_secs(900), "marimo didn't start: {said}");
        std::thread::sleep(Duration::from_millis(500));
    };
    let notebook = opened["notebook_id"].as_str().unwrap_or_else(|| panic!("{opened}")).to_owned();
    assert_eq!(opened["path"], notebooks.join("growth.py").display().to_string(), "{opened}");
    let link = opened["browser_url"].as_str().unwrap_or_else(|| panic!("{opened}"));
    assert!(link.starts_with(&format!("http://localhost:{port}/marimo/?file=")), "{opened}");
    eprintln!("[{:?}] the Python notebook is open on the machine", started.elapsed());
    let order = front.ok("get_cell_order", json!({ "notebook_id": notebook }));
    let cells: Vec<String> = order["cell_ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
    front.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cells[0] }));
    front.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
    front.ok("run_all_cells", json!({ "notebook_id": notebook, "wait_for_completion": true }));
    let opened_links = std::fs::read_to_string(work.join("opened.txt")).unwrap();
    let with_token = opened_links.lines().last().filter(|line| line.starts_with(link) && line.contains("token=")).unwrap_or_else(|| panic!("the browser got the link with the token: {opened_links}"));
    let target = with_token.strip_prefix(&format!("http://localhost:{port}")).unwrap();
    let (line, head, _) = get(port, target, "");
    assert_eq!(line, "HTTP/1.1 303 See Other", "{head}");
    let cookie = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}")).split(';').next().unwrap().to_owned();
    let (line, head, body) = get(port, link.strip_prefix(&format!("http://localhost:{port}")).unwrap(), &format!("Cookie: {cookie}\r\nSec-Fetch-Site: none\r\n"));
    assert_eq!(line, "HTTP/1.1 200 OK", "{head}");
    assert!(body.contains("marimo"), "{}", &body[..body.len().min(300)]);
    eprintln!("[{:?}] marimo's page answers through this computer's port", started.elapsed());
    let read = front.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cells[1] }));
    assert!(read["errored"] == false && read["output"].as_str().unwrap_or_default().contains("42"), "{read}");
    eprintln!("[{:?}] a Python cell ran on the machine", started.elapsed());
    front.ok("stop_machine", json!({ "machine": "e2e-python" }));
    front.finish();
}
