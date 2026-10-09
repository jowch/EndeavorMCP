//! The machine tools of `endeavor mcp` (the front), driven over stdio as an
//! agent's harness drives it, against the helper with a local `sh` standing in
//! for ssh (`ENDEAVOR_TEST_SHELL`), a stand-in Julia under the real core, and
//! fake Slurm commands for a cluster. The front holds its connection to each
//! machine itself; what the tests look at is the tools' results, the helper
//! processes, the runtime and the machines and projects files. No sshd, Julia
//! or Slurm is needed. Everything is under `target/tmp`: the front's HOME,
//! state, config and cache folders, the helper's install and state folders,
//! and the ssh config that `list_machines` reads (it finds it through HOME).

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use common::front::Front;
use common::{FakeBridge, TOKEN, pid_alive, serving_julia, wait_for};
use endeavor_mcp::client::{Cluster, MachinesFile, Server};
use serde_json::{Value, json};
use wire::slurm::{Partition, Resources};

const NOTEBOOK: &str = "aaaaaaaa-0000-0000-0000-000000000001";

/// A machine called `lab` (or `hpc`, a cluster) whose helper and runtime live in a folder of the
/// test's own, and the folders of a front on "this computer" with a runtime of its own.
struct Place {
    dir: PathBuf,
    /// The front's project folder.
    project: PathBuf,
    /// The helper's state folder, where its runtime keeps `runtime.json`.
    state: PathBuf,
    /// The runtime of "this computer".
    local_state: PathBuf,
    bridge: FakeBridge,
    local_bridge: FakeBridge,
    julia: PathBuf,
    local_julia: PathBuf,
    /// What a front gets (it is given nothing else).
    env: Vec<(String, String)>,
}

impl Place {
    fn new(name: &str) -> Place {
        Place::with(name, &[])
    }

    /// With the helper installed on the machine already.
    fn with(name: &str, more: &[(&str, &str)]) -> Place {
        let place = Place::bare(name, more);
        common::install_helper(&place.dir.join("root"));
        place
    }

    /// With more variables, or other values for those set here; `{dir}` is this place's folder.
    /// The machine has no helper.
    fn bare(name: &str, more: &[(&str, &str)]) -> Place {
        common::require_debug_build();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("machines-{name}"));
        end_leftovers(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let (state, local_state, project) = (dir.join("runtime-state"), dir.join("local-state"), dir.join("project"));
        for folder in [&state, &local_state, &project, &dir.join("home/.ssh")] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let (bridge, local_bridge) = (FakeBridge::start(&state), FakeBridge::start(&local_state));
        let (julia, local_julia) = (serving_julia(&state, &bridge), serving_julia(&local_state, &local_bridge));
        for folder in [&state, &local_state] {
            std::fs::write(folder.join("token"), TOKEN).unwrap();
        }
        let path = |name: &str| dir.join(name).display().to_string();
        let mut env: Vec<(String, String)> = [
            ("PATH", std::env::var("PATH").unwrap()),
            ("HOME", path("home")),
            ("XDG_STATE_HOME", path("state-home")),
            ("XDG_CONFIG_HOME", path("config")),
            ("XDG_CACHE_HOME", path("cache")),
            ("ENDEAVOR_TEST_SHELL", "1".into()),
            ("ENDEAVOR_TEST_ROOT", path("root")),
            ("ENDEAVOR_TEST_STATE", state.display().to_string()),
            ("ENDEAVOR_TEST_DEPOT", path("depot")),
            ("ENDEAVOR_START_WAIT_SECS", "30".into()),
            ("ENDEAVOR_SLURM_POLL_MS", "100".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        for (name, value) in more {
            env.retain(|(n, _)| n != name);
            env.push((name.to_string(), value.replace("{dir}", &dir.display().to_string())));
        }
        let project = project.canonicalize().unwrap();
        Place { dir, project, state, local_state, bridge, local_bridge, julia, local_julia, env }
    }

    fn machines(&self) -> MachinesFile {
        MachinesFile::at(self.dir.join("config/endeavor/machines.json"))
    }

    /// A plain server called `lab`.
    fn add_lab(&self) {
        self.machines().save(Server { id: "lab".into(), name: "lab".into(), ssh_host: "lab".into(), julia: Some(self.julia.display().to_string()), ..Default::default() }).unwrap();
    }

    /// A cluster called `hpc`, with a default job of 8 CPUs, 32 GB and 8 hours.
    fn add_hpc(&self) {
        let resources = Resources { partition: None, cpus: 8, mem_gb: 32, minutes: 480, gres: None, extra: Vec::new() };
        let partitions = vec![Partition { name: "shared".into(), default: true, max_minutes: Some(1440), cpus: 32, mem_mb: 128 * 1024 }];
        let cluster = Cluster { resources, partitions, ..Default::default() };
        self.machines().save(Server { id: "hpc".into(), name: "hpc".into(), ssh_host: "hpc".into(), julia: Some(self.julia.display().to_string()), cluster: Some(cluster), ..Default::default() }).unwrap();
    }

    fn projects(&self) -> Value {
        std::fs::read_to_string(self.dir.join("state-home/endeavor/projects.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
    }

    fn runtime(&self) -> Option<i32> {
        recorded_pid(&self.state)
    }

    /// The runtime's own port on the machine, as its record says.
    fn runtime_port(&self) -> u16 {
        read_json_port(&self.state)
    }

    fn local_runtime(&self) -> Option<i32> {
        recorded_pid(&self.local_state)
    }

    /// The pids of the helper (`endeavor connect`) of this place's machine.
    fn helpers(&self) -> Vec<i32> {
        pids(&format!("connect --state-dir {}", self.state.display()))
    }

    /// The pids of every helper of this place, whichever machine it is for.
    fn all_helpers(&self) -> Vec<i32> {
        pids(&format!("connect --state-dir {}/", self.dir.display()))
    }

    /// A notebook file in the project, open in the machine's runtime.
    fn notebook(&self, name: &str) -> String {
        let path = self.project.join(name);
        std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
        let path = path.display().to_string();
        self.bridge.set_notebooks(vec![notebook_json(NOTEBOOK, &path)]);
        path
    }

    fn front(&self) -> Front {
        start_front(self, &[])
    }

    /// A front started with `--no-folder`, in the project folder.
    fn front_without_folder(&self) -> Front {
        spawn_front(self, &["--no-folder"], &[])
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        end_leftovers(&self.dir);
    }
}

fn notebook_json(id: &str, path: &str) -> Value {
    json!({
        "notebook_id": id, "path": path, "cell_order": ["c1"], "execution_allowed": true, "safe_preview": false, "pending_run": [],
        "cells": [{ "cell_id": "c1", "code": "x = 1", "running": false, "queued": false, "errored": false }],
    })
}

fn recorded_pid(state: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(state.join("runtime.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["pid"].as_i64().map(|p| p as i32).filter(|&p| p > 1)
}

fn read_record(state: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(state.join("runtime.json")).expect("a runtime record")).unwrap()
}

fn read_json_port(state: &Path) -> u16 {
    let text = std::fs::read_to_string(state.join("runtime.json")).expect("a runtime record");
    serde_json::from_str::<Value>(&text).unwrap()["port"].as_u64().expect("a port in the record") as u16
}

fn pids(pattern: &str) -> Vec<i32> {
    let found = Command::new("pgrep").arg("-f").arg("--").arg(pattern).output().unwrap();
    String::from_utf8_lossy(&found.stdout).split_whitespace().filter_map(|p| p.parse().ok()).collect()
}

/// End what a test of `dir` started: the helpers its fronts left (by their pids), then its runtimes (a core, Julia and its workers are one process group).
fn end_leftovers(dir: &Path) {
    let helpers: Vec<i32> = pids(&format!("connect --state-dir {}/", dir.display()));
    for &pid in &helpers {
        // SAFETY: plain syscall, on a helper this test started: its command line holds this test's own folder.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    for pid in helpers {
        wait_for("the helper to end", || !pid_alive(pid));
    }
    // The recorded runtimes, and any still starting (no record yet): the cores whose command line is `core --state-dir` and this test's own folders.
    let states: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("runtime-state") || n == "local-state")).collect();
    let mut groups: Vec<i32> = Vec::new();
    for state in &states {
        groups.extend(pids(&format!("core --state-dir {} ", state.display())));
    }
    groups.sort();
    groups.dedup();
    for &pid in &groups {
        // SAFETY: plain syscalls, on a core this test started and the processes in its group.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
            libc::kill(pid, libc::SIGTERM);
        }
    }
    // Gone before the next test uses the folder: a dying core could write into it.
    let limit = Instant::now() + Duration::from_secs(5);
    while Instant::now() < limit && groups.iter().any(|&pid| pid_alive(pid) || unsafe { libc::kill(-pid, 0) } == 0) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_front(place: &Place, more: &[(&str, &str)]) -> Front {
    spawn_front(place, &["--folder", &place.project.display().to_string()], more)
}

/// A front with `folder_args` (`--folder DIR` or `--no-folder`), whose working folder is the project.
fn spawn_front(place: &Place, folder_args: &[&str], more: &[(&str, &str)]) -> Front {
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command
        .args(["mcp", "--skills", "plugin"])
        .args(folder_args)
        .arg("--julia")
        .arg(&place.local_julia)
        .arg("--depot")
        .arg(place.dir.join("local-depot"))
        .arg("--state-dir")
        .arg(&place.local_state)
        .env_clear()
        .envs(place.env.iter().map(|(k, v)| (k, v)))
        .envs(more.iter().copied())
        .current_dir(&place.project);
    Front::spawn(command)
}

/// One request to `port`: the status code and the body.
fn http(port: u16, request: &str) -> (u16, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    socket.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    let _ = socket.read_to_string(&mut reply);
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0), body.to_owned())
}

/// Another agent session's tool call, straight to a runtime's port. The tool's result.
fn other_agent(port: u16, token: &str, session: &str, name: &str, arguments: Value) -> Value {
    let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string();
    let request = format!(
        "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-Endeavor-Session: {session}\r\nContent-Length: {}\r\n\r\n{message}",
        message.len()
    );
    let (code, body) = http(port, &request);
    assert_eq!(code, 200, "{body}");
    let reply: Value = serde_json::from_str(&body).unwrap();
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{reply}"));
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// What a failed call said: the plain text of a call that couldn't reach a runtime, or the message of a tool's own error.
fn text(said: &Value) -> &str {
    said.as_str().or_else(|| said["message"].as_str()).unwrap_or_else(|| panic!("no message in {said}"))
}

fn this_host() -> String {
    String::from_utf8(Command::new("hostname").output().unwrap().stdout).unwrap().trim().to_owned()
}

/// The port in a `browser_url`.
fn url_port(url: &Value) -> u16 {
    url.as_str().and_then(|u| u.strip_prefix("http://localhost:")).and_then(|u| u.split(['/', '?']).next()).and_then(|p| p.parse().ok()).unwrap_or_else(|| panic!("not a browser url: {url}"))
}

#[test]
fn the_tool_list_has_the_host_tools_and_the_four_machine_tools() {
    let place = Place::new("tools");
    let mut front = place.front();
    let init = front.initialize();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("`list_machines`, `add_machine`, `use_machine` and `stop_machine`"), "{instructions}");
    let reply = front.request("tools/list", json!({}));
    let tools = reply["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for name in ["list_notebooks", "new_notebook", "list_folder", "read_file", "run_shell", "list_machines", "add_machine", "use_machine", "stop_machine"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    assert!(!names.contains(&"notebook_guide"), "the agent has the plugin's skills");
    let read_only = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"].clone();
    assert_eq!([read_only("list_machines"), read_only("add_machine"), read_only("use_machine"), read_only("stop_machine"), read_only("run_shell")], [true, false, false, false, false]);
    // On this computer the host tools refuse, as the runtime says.
    let (failed, refused) = front.call("list_folder", json!({ "path": "/" }));
    assert!(failed && refused["message"].as_str().unwrap().contains("only for sessions on a server"), "{refused}");
    assert!(place_none(&place), "the refusal started no runtime");
}

#[test]
fn add_machine_reports_and_saves_a_cluster_and_list_machines_connects_to_nothing() {
    let slurm = FakeSlurm::new("add");
    let place = Place::with("add", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    std::fs::write(place.dir.join("home/.ssh/config"), "Host hpc other\n  HostName example.org\nHost *.edu\n  User ada\n").unwrap();
    let mut front = place.front();
    front.initialize();

    let listed = front.ok("list_machines", json!({}));
    assert_eq!(listed["machines"], json!([]));
    assert_eq!(listed["this_session"]["machine"], "local");
    assert_eq!(listed["local"]["name"], "local");
    assert_eq!(listed["local"]["this_session"], true);
    assert_eq!(listed["ssh_hosts_not_added"], json!(["hpc", "other"]));

    let julia = place.julia.display().to_string();
    let added = front.ok("add_machine", json!({ "host": "hpc", "julia": julia }));
    assert_eq!((added["machine"].as_str(), added["host"].as_str(), added["state"].as_str(), added["saved"].clone()), (Some("hpc"), Some("hpc"), Some("connected"), json!(true)));
    assert_eq!(added["node"], this_host());
    assert_eq!(added["home"], place.dir.join("home").display().to_string());
    assert_eq!(added["slurm"], true);
    assert_eq!(added["partitions"], json!([{ "name": "shared", "default": true, "max_hours": 8.0, "cpus": 10, "memory_gb": 7 }]));
    assert!(added["message"].as_str().unwrap().contains("It has Slurm, and Julia runs in Slurm jobs there. Partitions: shared (default) (up to 8 h, 10 CPUs and 7 GB a node)"), "{}", added["message"]);
    assert!(added["message"].as_str().unwrap().contains("call `add_machine` again with slurm false"), "the other way is offered: {}", added["message"]);
    assert_eq!((added["cluster"].clone(), added["runs_in"].clone()), (json!(true), json!("slurm_jobs")));

    let saved = place.machines().find_by_name("hpc").unwrap().expect("saved");
    assert_eq!((saved.id.as_str(), saved.ssh_host.as_str(), saved.julia.as_deref()), ("hpc", "hpc", Some(julia.as_str())));
    let cluster = saved.cluster.expect("a cluster, since Slurm is there");
    assert_eq!(cluster.partitions.len(), 1);
    assert_eq!((cluster.resources.cpus, cluster.resources.mem_gb, cluster.resources.minutes), (8, 7, 480), "a medium job, kept within the partition");
    // It connected for Slurm jobs from the start, so the connection is kept for the next call.
    assert_eq!(place.helpers().len(), 1, "one helper");

    let listed = front.ok("list_machines", json!({}));
    assert_eq!(listed["machines"], json!([{ "name": "hpc", "host": "hpc", "cluster": true, "state": "connected", "this_session": false }]));
    assert_eq!(listed["ssh_hosts_not_added"], json!(["other"]));

    // Adding it again updates it.
    let again = front.ok("add_machine", json!({ "host": "hpc" }));
    assert_eq!((again["updated"].clone(), again["partitions"].clone()), (json!(true), added["partitions"].clone()));
    assert_eq!(place.machines().load().unwrap().len(), 1);
    // A name that isn't plain, and a host that ssh would take as an option, are refused before anything runs.
    let (failed, refused) = front.call("add_machine", json!({ "host": "-oProxyCommand=x" }));
    assert!(failed && refused["message"].as_str().unwrap().contains("isn't an SSH host name"), "{refused}");
    let (failed, refused) = front.call("add_machine", json!({ "host": "lab", "name": "local" }));
    assert!(failed && refused["message"].as_str().unwrap().contains("is this computer"), "{refused}");
    assert_eq!(place.machines().load().unwrap().len(), 1);
}

#[test]
fn adding_a_plain_server_again_keeps_it_plain_even_when_slurm_is_there() {
    let slurm = FakeSlurm::new("keep-plain");
    let place = Place::with("keep-plain", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    let added = front.ok("add_machine", json!({ "host": "lab" }));
    assert_eq!((added["slurm"].clone(), added["updated"].clone(), added["partitions"].as_array().map(Vec::len)), (json!(true), json!(true), Some(1)), "{added}");
    assert!(added["message"].as_str().unwrap().contains("saved before as a plain server") && added["message"].as_str().unwrap().contains("slurm true"), "{added}");
    assert_eq!((added["cluster"].clone(), added["runs_in"].clone()), (json!(false), json!("directly")));
    assert!(place.machines().find_by_name("lab").unwrap().unwrap().cluster.is_none(), "still a plain server");
    assert_eq!(front.ok("list_machines", json!({}))["machines"][0]["state"], "connected", "and its connection is kept");
    assert!(!place.helpers().is_empty(), "with its helper");
}

#[test]
fn a_new_machine_with_slurm_tools_is_added_as_a_plain_server_when_slurm_is_false() {
    let slurm = FakeSlurm::new("plain-by-choice");
    let place = Place::with("plain-by-choice", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let (failed, said) = front.call("add_machine", json!({ "host": "lab", "julia": julia, "slurm": "yes" }));
    assert!(failed && text(&said).contains("slurm must be true"), "{said}");
    assert!(place.machines().load().unwrap().is_empty());

    let added = front.ok("add_machine", json!({ "host": "lab", "julia": julia, "slurm": false }));
    assert_eq!((added["slurm"].clone(), added["cluster"].clone(), added["runs_in"].clone(), added["updated"].clone()), (json!(true), json!(false), json!("directly"), json!(false)), "{added}");
    let message = added["message"].as_str().unwrap();
    assert!(message.contains("It has Slurm, but Julia runs on it directly and not in a job, as asked") && message.contains("`add_machine` again with slurm true"), "{message}");
    assert!(place.machines().find_by_name("lab").unwrap().unwrap().cluster.is_none(), "saved as a plain server");
    let helpers = place.helpers();

    // Used, it starts Julia directly: no job is submitted, and the connection that added it is the one used.
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((used["state"].as_str(), used["ready"].clone()), (Some("ready"), json!(true)), "{used}");
    assert_eq!(place.helpers(), helpers, "no second connection was needed");
    assert_eq!(slurm.read("sbatch.args"), "", "nothing was submitted");

    // Adding it again with nothing said leaves it as it was; saying slurm true while Julia runs there is refused.
    let again = front.ok("add_machine", json!({ "host": "lab" }));
    assert_eq!(again["cluster"], false, "{again}");
    let (failed, said) = front.call("add_machine", json!({ "host": "lab", "slurm": true }));
    assert!(failed && text(&said).contains("Julia is running, or starting, on lab") && text(&said).contains("`stop_machine`"), "{said}");
    assert!(place.machines().find_by_name("lab").unwrap().unwrap().cluster.is_none(), "unchanged");
    assert_eq!(front.ok("list_notebooks", json!({})), json!([]), "and the session still works");

    // With nothing running, it can be changed to a cluster, and back.
    front.ok("stop_machine", json!({ "machine": "lab" }));
    let cluster = front.ok("add_machine", json!({ "host": "lab", "slurm": true }));
    assert_eq!((cluster["cluster"].clone(), cluster["updated"].clone()), (json!(true), json!(true)), "{cluster}");
    assert!(cluster["message"].as_str().unwrap().contains("slurm false"), "{cluster}");
    assert!(place.machines().find_by_name("lab").unwrap().unwrap().cluster.is_some());
    let plain = front.ok("add_machine", json!({ "host": "lab", "slurm": false }));
    assert_eq!(plain["cluster"], false, "{plain}");
    assert!(place.machines().find_by_name("lab").unwrap().unwrap().cluster.is_none());
}

#[test]
fn a_failing_add_machine_leaves_no_record_and_no_connection() {
    let place = Place::with("failadd", &[("ENDEAVOR_TEST_ASK", "echo 'Permission denied (publickey)' >&2; false")]);
    let mut front = place.front();
    front.initialize();
    let (failed, said) = front.call("add_machine", json!({ "host": "lab", "name": "failadd", "julia": place.julia.display().to_string() }));
    let message = text(&said);
    assert!(failed && message.contains("Couldn't connect to lab") && message.contains("Nothing was saved") && message.contains("never ask them for a password"), "{said}");
    assert!(!place.machines().path().exists(), "no record, no file");
    wait_for("the helper to go", || place.helpers().is_empty());

    // A machine that was there before keeps its record as it was.
    place.machines().save(Server { id: "failadd".into(), name: "failadd".into(), ssh_host: "lab".into(), ..Default::default() }).unwrap();
    let before = place.machines().load().unwrap();
    let (failed, _) = front.call("add_machine", json!({ "host": "lab2", "name": "failadd" }));
    assert!(failed);
    assert_eq!(place.machines().load().unwrap(), before);
    assert_eq!(front.ok("list_machines", json!({}))["machines"][0]["state"], "not connected", "the connection that failed was not kept");
}

#[test]
fn fields_the_front_does_not_know_survive_adding_and_updating_machines() {
    let place = Place::new("unknown-fields");
    let julia = place.julia.display().to_string();
    let path = place.machines().path().to_owned();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let before = json!({
        "schema": 1, "app": {"theme": "dark"},
        "machines": [
            {"id": "lab", "name": "lab", "ssh_host": "lab", "julia": julia, "color": "red"},
            {"id": "other", "name": "other", "ssh_host": "other", "color": "blue", "tags": ["a"]},
        ],
    });
    std::fs::write(&path, before.to_string()).unwrap();
    let mut front = place.front();
    front.initialize();
    let raw = || serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();

    let added = front.ok("add_machine", json!({ "host": "box", "name": "box", "julia": julia }));
    assert_eq!(added["state"], "connected", "{added}");
    let now = raw();
    assert_eq!((now["app"].clone(), now["machines"][0]["color"].clone(), now["machines"][1]["tags"].clone()), (json!({"theme": "dark"}), json!("red"), json!(["a"])), "{now}");
    assert_eq!(now["machines"][2]["id"], "box");

    let updated = front.ok("add_machine", json!({ "host": "lab", "name": "lab", "julia": julia, "install": false }));
    assert_eq!((updated["state"].as_str(), updated["updated"].clone()), (Some("connected"), json!(true)), "{updated}");
    let now = raw();
    assert_eq!((now["app"].clone(), now["machines"][0]["color"].clone(), now["machines"][1]["color"].clone()), (json!({"theme": "dark"}), json!("red"), json!("blue")), "{now}");
}

#[test]
fn a_machines_file_of_a_newer_schema_is_listed_and_not_rewritten() {
    let place = Place::new("newer-schema");
    let path = place.machines().path().to_owned();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let content = json!({ "schema": 2, "machines": [{"id": "lab", "name": "lab", "ssh_host": "lab", "julia": place.julia.display().to_string()}] }).to_string();
    std::fs::write(&path, &content).unwrap();
    let mut front = place.front();
    front.initialize();
    let listed = front.ok("list_machines", json!({}));
    assert_eq!(listed["machines"][0]["name"], "lab", "{listed}");
    let (failed, said) = front.call("add_machine", json!({ "host": "box", "name": "box" }));
    assert!(failed && text(&said).contains("A newer Endeavor wrote"), "{said}");
    assert!(place.helpers().is_empty(), "it didn't connect before it found out");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content, "untouched");
}

#[test]
fn a_tool_acts_on_the_saved_settings_when_an_add_for_others_never_connected() {
    let (place, _slurm) = slow_place("host-b", "");
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    // The new host doesn't connect in time: it stays unsaved, and the connection for it waits for the next call.
    let other = front.ok("add_machine", json!({ "host": "labb", "name": "lab", "julia": julia }));
    assert_eq!((other["state"].as_str(), other["saved"].clone()), (Some("connecting"), json!(true)), "{other}");
    assert!(other["message"].as_str().unwrap().contains("stays as it was"), "{other}");
    assert_eq!(place.machines().find_by_name("lab").unwrap().unwrap().ssh_host, "lab", "the file still has the old settings");
    wait_for("the attempt for the other host", || attempts(&place) == 1);

    // A tool asks for the saved record, and gets a connection made from it; the attempt for the other host is let go.
    std::fs::write(place.dir.join("go"), "").unwrap();
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(used["state"], "ready", "{used}");
    assert_eq!(attempts(&place), 2, "a connection of its own for the saved settings");
    assert_eq!(front.ok("list_machines", json!({}))["machines"][0]["host"], "lab");
    front.finish();
}

#[test]
fn settings_that_changed_under_a_runtime_in_use_are_refused_in_plain_words() {
    let place = Place::new("host-b-busy");
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(used["state"], "ready");
    let helpers = place.helpers();
    let (failed, said) = front.call("add_machine", json!({ "host": "labb", "name": "lab", "julia": place.julia.display().to_string() }));
    assert!(failed && text(&said).contains("changed while Julia is in use") && text(&said).contains("stop_machine"), "{said}");
    assert!(!text(&said).contains("link"), "{said}");
    assert_eq!(place.helpers(), helpers, "the connection stays");
    assert_eq!(place.machines().find_by_name("lab").unwrap().unwrap().ssh_host, "lab");
    // The file's record is the one the tools use, so they keep working.
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!((status["machine"].as_str(), url_port(&status["browser_url"])), (Some("lab"), url_port(&used["browser_url"])));
    front.finish();
}

#[test]
fn a_session_on_a_working_connection_does_not_need_the_machines_file() {
    let place = Place::new("file-gone");
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    front.ok("use_machine", json!({ "machine": "lab" }));
    let path = place.machines().path().to_owned();
    let text = std::fs::read_to_string(&path).unwrap();
    place.machines().remove("lab").unwrap();
    assert_eq!(front.ok("pluto_session_status", json!({}))["machine"], "lab", "the machine left the list");
    std::fs::write(&path, "{not json").unwrap();
    assert_eq!(front.ok("pluto_session_status", json!({}))["machine"], "lab", "the file is broken for a moment");
    std::fs::write(&path, text).unwrap();
    front.finish();
}

#[test]
fn a_list_that_changed_while_connecting_is_not_saved_over_and_the_new_machines_connection_ends() {
    let (place, _slurm) = slow_place("changed-meanwhile", "");
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let interfere = {
        let (machines, asking, go) = (place.machines(), place.dir.join("asking"), place.dir.join("go"));
        std::thread::spawn(move || {
            wait_for("the connection to start", || asking.exists());
            machines.save(Server { id: "box".into(), name: "someone-elses".into(), ssh_host: "elsewhere".into(), ..Default::default() }).unwrap();
            std::fs::write(go, "").unwrap();
        })
    };
    let (failed, said) = front.call("add_machine", json!({ "host": "lab", "name": "box", "julia": julia }));
    interfere.join().unwrap();
    assert!(failed && text(&said).contains("changed while Endeavor was connecting") && text(&said).contains("add_machine"), "{said}");
    let saved = place.machines().load().unwrap();
    assert_eq!(saved.iter().map(|s| (s.id.as_str(), s.name.as_str())).collect::<Vec<_>>(), [("box", "someone-elses")], "the other record is intact");
    wait_for("the connection to end", || place.helpers().is_empty());
    assert_eq!(front.ok("list_machines", json!({}))["machines"][0]["state"], "not connected");
    front.finish();
}

#[test]
fn a_file_of_a_newer_shape_answers_every_tool_with_the_newer_message() {
    let place = Place::new("newer-shape");
    let path = place.machines().path().to_owned();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let content = r#"{"schema": 2, "hosts": []}"#;
    std::fs::write(&path, content).unwrap();
    let mut front = place.front();
    front.initialize();
    for (tool, args) in [("list_machines", json!({})), ("add_machine", json!({ "host": "lab" })), ("use_machine", json!({ "machine": "lab" }))] {
        let (failed, said) = front.call(tool, args);
        assert!(failed && text(&said).contains("A newer Endeavor wrote") && !text(&said).contains("remove the file"), "{tool}: {said}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    front.finish();
}

#[test]
fn fields_inside_a_cluster_survive_the_changes_the_front_makes() {
    let slurm = FakeSlurm::new("cluster-unknown");
    let place = Place::with("cluster-unknown", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_hpc();
    let path = place.machines().path().to_owned();
    let mut raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    raw["machines"][0]["cluster"]["qos"] = "long".into();
    raw["machines"][0]["cluster"]["resources"]["priority"] = 3.into();
    raw["machines"][0]["cluster"]["partitions"][0]["features"] = json!(["a100"]);
    std::fs::write(&path, raw.to_string()).unwrap();
    let kept = || -> bool {
        let now: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let cluster = &now["machines"][0]["cluster"];
        cluster["qos"] == "long" && cluster["resources"]["priority"] == 3 && cluster["partitions"][0]["features"] == json!(["a100"])
    };
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();

    let queued = front.ok("use_machine", json!({ "machine": "hpc", "cpus": 4, "memory_gb": 16, "hours": 2, "partition": "shared" }));
    assert_eq!(queued["state"], "queued", "{queued}");
    assert_eq!(place.machines().find_by_name("hpc").unwrap().unwrap().cluster.unwrap().resources.cpus, 4, "the defaults were saved");
    assert!(kept(), "after use_machine saved the defaults");

    let added = front.ok("add_machine", json!({ "host": "other", "name": "other", "julia": julia }));
    assert_eq!(added["state"], "connected", "{added}");
    assert!(kept(), "after another machine was added");

    let updated = front.ok("add_machine", json!({ "host": "hpc", "name": "hpc", "julia": julia }));
    assert_eq!((updated["state"].as_str(), updated["cluster"].clone()), (Some("connected"), json!(true)), "{updated}");
    assert!(kept(), "after the machine was updated and stayed a cluster");
    front.finish();
}

/// How many connections were tried (`slow_place`).
fn attempts(place: &Place) -> usize {
    std::fs::read_to_string(place.dir.join("attempts")).map_or(0, |text| text.lines().count())
}

/// A place whose connections wait for the file `go` before they connect, with Slurm's commands on the PATH. Each attempt is noted in `attempts` and, while it waits, in `asking`.
fn slow_place(name: &str, then: &str) -> (Place, FakeSlurm) {
    let slurm = FakeSlurm::new(name);
    let ask = format!("echo x >> {{dir}}/attempts; touch {{dir}}/asking; while [ ! -f {{dir}}/go ]; do sleep 0.1; done{then}");
    let place = Place::with(name, &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string()), ("ENDEAVOR_START_WAIT_SECS", "2"), ("ENDEAVOR_TEST_ASK", &ask)]);
    (place, slurm)
}

#[test]
fn an_add_machine_that_is_still_connecting_saves_nothing_until_a_second_call_has_connected() {
    let (place, _slurm) = slow_place("connecting", "");
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let first = front.ok("add_machine", json!({ "host": "lab", "julia": julia }));
    assert_eq!((first["state"].as_str(), first["saved"].clone()), (Some("connecting"), json!(false)), "{first}");
    assert!(first["message"].as_str().unwrap().contains("saved when it has connected"), "{first}");
    assert!(!place.machines().path().exists(), "nothing in the machines file");
    assert_eq!(attempts(&place), 1);

    assert_eq!(front.ok("list_machines", json!({}))["machines"], json!([]));
    let (failed, said) = front.call("use_machine", json!({ "machine": "lab" }));
    assert!(failed && said["error"] == "machine_not_found", "{said}");
    assert_eq!(place.projects(), Value::Null);

    // Other settings while it is still connecting replace the connection, which was made for the first ones.
    let other = front.ok("add_machine", json!({ "host": "lab2", "name": "lab", "julia": julia }));
    assert_eq!((other["state"].as_str(), other["saved"].clone()), (Some("connecting"), json!(false)), "{other}");
    assert_eq!(attempts(&place), 2, "a new connection");
    assert!(!place.machines().path().exists());

    // The next call finishes it, and treats it as new: Slurm is found, so it is a cluster.
    std::fs::write(place.dir.join("go"), "").unwrap();
    let done = front.ok("add_machine", json!({ "host": "lab2", "name": "lab", "julia": julia }));
    assert_eq!((done["state"].as_str(), done["updated"].clone(), done["cluster"].clone()), (Some("connected"), json!(false), json!(true)), "{done}");
    assert_eq!(attempts(&place), 2, "the call went on with the connection the one before left");
    assert_eq!(place.machines().load().unwrap().len(), 1, "one record");
    assert!(place.machines().find_by_name("lab").unwrap().unwrap().cluster.is_some());
    let listed = front.ok("list_machines", json!({}));
    assert_eq!((listed["machines"][0]["cluster"].clone(), listed["machines"][0]["state"].clone()), (json!(true), json!("connected")), "{listed}");
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(used["state"], "needs_job", "{used}");
    assert_eq!(attempts(&place), 2, "use_machine went on with the connection add_machine made: no second sign-in");
}

#[test]
fn an_add_machine_saying_slurm_false_after_one_still_connecting_connects_again_and_submits_nothing() {
    let (place, slurm) = slow_place("connecting-then-plain", "");
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let first = front.ok("add_machine", json!({ "host": "lab", "julia": julia }));
    assert_eq!(first["state"], "connecting", "{first}");
    std::fs::write(place.dir.join("go"), "").unwrap();
    // That connection settles as Slurm (sinfo is on the PATH); the user says it isn't a cluster.
    let added = front.ok("add_machine", json!({ "host": "lab", "julia": julia, "slurm": false }));
    assert_eq!((added["state"].as_str(), added["cluster"].clone(), added["runs_in"].clone()), (Some("connected"), json!(false), json!("directly")), "{added}");
    assert_eq!(attempts(&place), 2, "a new connection for the launcher asked for, not the one made for auto");
    assert!(place.machines().find_by_name("lab").unwrap().unwrap().cluster.is_none(), "saved as a plain server");
    assert_eq!(slurm.read("sbatch.args"), "", "nothing was submitted");
    assert_eq!(front.ok("list_machines", json!({}))["machines"][0]["state"], "connected", "and the connection it made is kept");
}

#[test]
fn a_second_add_machine_that_fails_leaves_the_machines_file_as_it_was() {
    let (place, _slurm) = slow_place("connecting-fails", "; echo 'Permission denied (publickey)' >&2; false");
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let first = front.ok("add_machine", json!({ "host": "lab", "julia": julia }));
    assert_eq!(first["state"], "connecting", "{first}");
    assert!(!place.machines().path().exists());
    std::fs::write(place.dir.join("go"), "").unwrap();
    let (failed, said) = front.call("add_machine", json!({ "host": "lab", "julia": julia }));
    assert!(failed && text(&said).contains("Couldn't connect to lab") && text(&said).contains("Nothing was saved"), "{said}");
    assert!(!place.machines().path().exists(), "no file was made");
    assert_eq!(front.ok("list_machines", json!({}))["machines"], json!([]));
}

#[test]
fn a_use_machine_that_fails_leaves_the_session_its_key_and_the_project_as_they_were() {
    let slurm = FakeSlurm::new("use-fails");
    let place = Place::with("use-fails", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_lab();
    place.add_hpc();
    let local_file = place.project.join("local.jl");
    std::fs::write(&local_file, "### A Pluto.jl notebook ###").unwrap();
    place.local_bridge.set_notebooks(vec![notebook_json("bbbbbbbb-0000-0000-0000-000000000002", &local_file.display().to_string())]);
    let mut front = place.front();
    front.initialize();
    front.ok("open_notebook", json!({ "path": local_file.display().to_string() }));
    let bound = |front: &mut Front, path: &str| {
        let listed = front.ok("list_notebooks", json!({}));
        assert_eq!((listed[0]["path"].as_str(), listed[0]["this_session"].clone()), (Some(path), json!(true)), "the notebook is still this session's: {listed}");
    };
    bound(&mut front, &local_file.display().to_string());

    for arguments in [
        json!({ "machine": "hpc", "partition": "nope" }),
        json!({ "machine": "hpc", "cpus": 4, "extra_sbatch_flags": ["--wrap=sleep 1"] }),
        json!({ "machine": "hpc", "cpus": 4, "extra_sbatch_flags": ["normal"] }),
        json!({ "machine": "lab", "cpus": 4 }),
    ] {
        let (failed, said) = front.call("use_machine", arguments.clone());
        assert!(failed, "{arguments}: {said}");
        bound(&mut front, &local_file.display().to_string());
        assert_eq!(place.projects(), Value::Null, "nothing written for {arguments}");
    }
    assert!(place.helpers().is_empty(), "no connection was even made");
    assert_eq!(slurm.read("sbatch.args"), "");

    // On a machine, the same.
    let path = place.notebook("machine.jl");
    front.ok("use_machine", json!({ "machine": "lab" }));
    front.ok("open_notebook", json!({ "path": path }));
    bound(&mut front, &path);
    let remembered = std::fs::read(place.dir.join("state-home/endeavor/projects.json")).unwrap();
    let (failed, said) = front.call("use_machine", json!({ "machine": "hpc", "partition": "nope" }));
    assert!(failed && text(&said).contains("There is no partition \"nope\""), "{said}");
    bound(&mut front, &path);
    assert_eq!(front.ok("pluto_session_status", json!({}))["machine"], "lab");
    assert_eq!(std::fs::read(place.dir.join("state-home/endeavor/projects.json")).unwrap(), remembered, "projects.json is byte for byte what it was");
}

#[test]
fn use_machine_puts_the_session_on_the_machine_and_local_puts_it_back() {
    let place = Place::new("use");
    place.add_lab();
    let machine_notebook = place.notebook("machine.jl");
    let local_file = place.project.join("local.jl");
    std::fs::write(&local_file, "### A Pluto.jl notebook ###").unwrap();
    place.local_bridge.set_notebooks(vec![notebook_json("bbbbbbbb-0000-0000-0000-000000000002", &local_file.display().to_string())]);
    std::fs::create_dir_all(place.dir.join("home/work")).unwrap();
    let mut front = place.front();
    front.initialize();

    let used = front.ok("use_machine", json!({ "machine": "lab", "folder": place.dir.join("home/work").display().to_string() }));
    assert_eq!((used["state"].as_str(), used["ready"].clone(), used["already_running"].clone()), (Some("ready"), json!(true), json!(false)), "{used}");
    assert_eq!(used["node"], this_host());
    assert_eq!(used["folder"], place.dir.join("home/work").display().to_string());
    let port = url_port(&used["browser_url"]);
    assert_eq!(used["browser_url"], format!("http://localhost:{port}/?token={TOKEN}"));
    assert!(used["message"].as_str().unwrap().contains("no notebook on lab yet"));

    // The agent's calls go to the machine's runtime, with its host and browser port.
    let listed = front.ok("list_notebooks", json!({}));
    assert_eq!(listed[0]["path"], machine_notebook, "the machine's notebooks, not this computer's: {listed}");
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!(url_port(&status["browser_url"]), port, "the connection's port, which the user's browser reaches");
    assert_eq!(status["machine"], "lab");
    assert_eq!((&status["exits_when_idle"], &status["idle_stop_hours"], status.get("message")), (&json!(true), &json!(48.0), None), "a runtime a session starts exits when idle: {status}");
    assert_eq!(read_record(&place.state)["exits_when_idle"], true);
    let folder = front.ok("list_folder", json!({ "path": place.project.display().to_string() }));
    assert!(folder.to_string().contains("machine.jl"), "{folder}");
    let shell = front.ok("run_shell", json!({ "command": "pwd; echo on-the-machine" }));
    assert!(shell.to_string().contains(&place.dir.join("home/work").display().to_string()) && shell.to_string().contains("on-the-machine"), "the session's folder there: {shell}");
    assert_eq!(place.projects()[place.project.display().to_string()], json!({ "machine": "lab", "folder": place.dir.join("home/work").display().to_string() }));

    // Back on this computer: its runtime, its notebooks, and the host tools refuse again.
    let back = front.ok("use_machine", json!({ "machine": "local" }));
    assert_eq!((back["machine"].as_str(), back["state"].as_str()), (Some("local"), Some("ready")), "{back}");
    let listed = front.ok("list_notebooks", json!({}));
    assert_eq!(listed[0]["path"], local_file.display().to_string(), "this computer's notebooks: {listed}");
    let local_port = url_port(&front.ok("pluto_session_status", json!({}))["browser_url"]);
    assert_ne!(local_port, port);
    assert_eq!(read_record(&place.local_state)["exits_when_idle"], true, "so does the one `mcp` starts on this computer");
    assert!(front.ok("pluto_session_status", json!({})).get("machine").is_none());
    let (failed, refused) = front.call("list_folder", json!({ "path": "/" }));
    assert!(failed && refused["message"].as_str().unwrap().contains("only for sessions on a server"), "{refused}");
    assert_eq!(place.projects().get(place.project.display().to_string()), None, "local clears what the project remembers");
    assert!(pid_alive(place.runtime().unwrap()), "leaving a machine doesn't stop its runtime");

    // And to the machine again.
    let again = front.ok("use_machine", json!({ "machine": "LAB" }));
    assert_eq!((again["state"].as_str(), again["already_running"].clone()), (Some("ready"), json!(true)), "{again}");
    let (failed, unknown) = front.call("use_machine", json!({ "machine": "nowhere" }));
    assert!(failed && unknown["message"].as_str().unwrap().contains("There is no machine \"nowhere\". Machines: lab."), "{unknown}");
}

#[test]
fn a_second_front_in_the_same_project_comes_up_on_the_remembered_machine() {
    let place = Place::new("remember");
    place.add_lab();
    let work = place.dir.join("home/work");
    std::fs::create_dir_all(&work).unwrap();
    let mut first = place.front();
    first.initialize();
    first.ok("use_machine", json!({ "machine": "lab", "folder": work.display().to_string() }));
    let runtime = place.runtime().unwrap();
    first.finish();
    assert!(pid_alive(runtime), "the front's exit leaves the runtime");

    let mut second = place.front();
    second.initialize();
    let status = second.ok("pluto_session_status", json!({}));
    assert_eq!(status["machine"], "lab", "attached without use_machine: {status}");
    let shell = second.ok("run_shell", json!({ "command": "pwd" }));
    assert!(shell.to_string().contains(&work.display().to_string()), "the folder there is remembered too: {shell}");
    assert_eq!(place.runtime(), Some(runtime), "the runtime that was running");
    assert!(!second.said().iter().any(|l| l.starts_with("Endeavor's notebooks:")), "nothing was started on this computer: {:?}", second.said());
    assert!(second.said().iter().any(|l| l.contains("this project uses the machine lab")));
}

#[test]
fn use_machine_without_a_folder_keeps_the_folder_the_project_remembers_for_that_machine() {
    let place = Place::new("remember-folder");
    place.add_lab();
    let (work, other) = (place.dir.join("home/work"), place.dir.join("home/other"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let project = place.project.display().to_string();
    let mut first = place.front();
    first.initialize();
    first.ok("use_machine", json!({ "machine": "lab", "folder": work.display().to_string() }));
    first.finish();

    let mut second = place.front();
    second.initialize();
    second.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(place.projects()[&project]["folder"], work.display().to_string().as_str(), "the project still remembers the folder");
    let shell = second.ok("run_shell", json!({ "command": "pwd" }));
    assert!(shell.to_string().contains(&work.display().to_string()), "the session is in the remembered folder: {shell}");

    second.ok("use_machine", json!({ "machine": "lab", "folder": other.display().to_string() }));
    assert_eq!(place.projects()[&project]["folder"], other.display().to_string().as_str(), "a folder that is given replaces it");
}

#[test]
fn use_machine_makes_a_folder_whose_parent_is_there_and_refuses_one_whose_parent_is_not_before_any_job() {
    let slurm = FakeSlurm::new("folder");
    let place = Place::with("folder", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_hpc();
    let mut front = place.front();
    front.initialize();
    let job = json!({ "cpus": 1, "memory_gb": 1, "hours": 1 });
    let ask = |folder: &Path| {
        let mut args = job.clone();
        args["machine"] = "hpc".into();
        args["folder"] = folder.display().to_string().into();
        args
    };

    // A folder whose parent is missing too, such as another computer's path: nothing is made and no job is asked for.
    let elsewhere = place.dir.join("Users/me/projects/study");
    let (failed, refused) = front.call("use_machine", ask(&elsewhere));
    let message = refused["message"].as_str().unwrap();
    assert!(failed && message.contains("can't be the session's folder: neither it nor the folder it would go in") && message.contains("exists on hpc") && message.contains("Nothing was started") && message.contains("another computer") && message.contains("Ask the user"), "{refused}");
    assert!(!place.dir.join("Users").exists(), "nothing was made");
    assert_eq!(slurm.read("sbatch.args"), "", "no job was submitted");
    assert_eq!(place.projects(), Value::Null, "the project remembers nothing");

    // A new folder whose parent is there is made, and the result says so.
    let study = place.dir.join("home/new-study");
    let queued = front.ok("use_machine", ask(&study));
    assert_eq!(queued["state"].as_str(), Some("queued"), "{queued}");
    assert!(study.is_dir(), "the folder was made");
    assert!(queued["message"].as_str().unwrap().contains(&format!("Made a new folder for the session on hpc: {}", study.display())), "{queued}");
    assert_ne!(slurm.read("sbatch.args"), "", "the job was submitted");
    assert_eq!(place.projects()[place.project.display().to_string()]["folder"], study.display().to_string().as_str());
}

#[test]
fn a_remembered_folder_that_is_gone_is_named_as_the_projects_and_a_given_folder_gets_past_it() {
    let place = Place::new("remembered-gone");
    place.add_lab();
    let work = place.dir.join("home/scratch/work");
    std::fs::create_dir_all(&work).unwrap();
    let mut first = place.front();
    first.initialize();
    first.ok("use_machine", json!({ "machine": "lab", "folder": work.display().to_string() }));
    first.finish();
    std::fs::remove_dir_all(place.dir.join("home/scratch")).unwrap();

    // Leaving `folder` out brings the remembered folder back, so the error says it is the project's and asks for another.
    let mut second = place.front();
    second.initialize();
    let (failed, refused) = second.call("use_machine", json!({ "machine": "lab" }));
    let message = refused["message"].as_str().unwrap();
    assert!(failed && message.contains(&format!("The folder this project used on lab, {},", work.display())) && message.contains("pass it as `folder`") && !message.contains("leave `folder` out"), "{refused}");
    let home = second.ok("use_machine", json!({ "machine": "lab", "folder": "~" }));
    assert_eq!(home["state"].as_str(), Some("ready"), "{home}");
    assert_eq!(place.projects()[place.project.display().to_string()]["folder"], "~", "the folder given replaces the one that was gone");
}

#[test]
fn a_remembered_plain_server_is_started_when_nothing_runs_there() {
    let place = Place::new("remember-start");
    place.add_lab();
    let mut first = place.front();
    first.initialize();
    first.ok("use_machine", json!({ "machine": "lab" }));
    let stopped = first.ok("stop_machine", json!({ "machine": "lab" }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    let (failed, said) = first.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains("was stopped from this session") && text(&said).contains("`use_machine` with machine \"lab\""), "{said}");
    let status = first.ok("pluto_session_status", json!({}));
    assert_eq!((status["state"].as_str(), status["ready"].clone()), (Some("stopped"), json!(false)), "{status}");
    first.finish();
    assert!(place.runtime().is_none());

    let mut second = place.front();
    second.initialize();
    assert_eq!(second.ok("list_notebooks", json!({})), json!([]));
    assert!(place.runtime().is_none(), "a query starts nothing");
    let path = place.notebook("a.jl");
    second.ok("open_notebook", json!({ "path": path }));
    assert!(place.runtime().is_some_and(pid_alive), "a notebook call started the runtime there, as asked last time");
}

#[test]
fn a_remembered_machine_that_is_gone_from_the_list_is_said_once() {
    let place = Place::new("remember-gone");
    place.add_lab();
    let mut first = place.front();
    first.initialize();
    first.ok("use_machine", json!({ "machine": "lab" }));
    first.finish();
    place.machines().remove("lab").unwrap();

    let mut second = place.front();
    second.initialize();
    let (failed, contents) = second.contents("list_notebooks", json!({}));
    assert!(!failed);
    assert_eq!(contents[0], json!([]), "this computer has no runtime and no notebooks");
    assert!(contents[1].as_str().unwrap().contains("last used the machine \"lab\", which isn't in the list of machines any more"), "{contents:?}");
    let (_, again) = second.contents("list_notebooks", json!({}));
    assert_eq!(again.len(), 1, "said once");
    assert!(place_none(&place), "no runtime was started for it");
}

#[test]
fn a_notebook_call_while_the_runtime_is_still_starting_returns_the_status_within_the_limit() {
    let place = Place::with("starting", &[("ENDEAVOR_START_WAIT_SECS", "3")]);
    place.add_lab();
    std::fs::write(place.state.join("hold"), "").unwrap();
    let mut front = place.front();
    front.initialize();
    let started = Instant::now();
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    assert_eq!((used["state"].as_str(), used["ready"].clone()), (Some("starting"), json!(false)), "{used}");
    assert!(used["message"].as_str().unwrap().contains("Julia is starting on lab") && used["message"].as_str().unwrap().contains("`pluto_session_status`"), "{used}");

    let started = Instant::now();
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    assert!(failed && text(&said).contains("Julia is starting on lab"), "{said}");
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!((status["machine"].as_str(), status["state"].as_str(), status["ready"].clone()), (Some("lab"), Some("starting"), json!(false)), "{status}");
    assert!(status["step"].is_string());

    // Julia is on its way, so it is not stopped without the user's word: others may be waiting for it.
    let refused = front.ok("stop_machine", json!({ "machine": "lab" }));
    assert_eq!((refused["stopped"].clone(), refused["state"].clone()), (json!(false), json!("starting")), "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("Julia is starting on lab") && refused["message"].as_str().unwrap().contains("force true"), "{refused}");
    assert_eq!(front.ok("pluto_session_status", json!({}))["state"], "starting", "nothing was cancelled");

    std::fs::remove_file(place.state.join("hold")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let status = front.ok("pluto_session_status", json!({}));
        if status.get("browser_url").is_some() {
            break status;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(status["machine"], "lab");
    assert_eq!(front.ok("list_notebooks", json!({})), json!([]));
}

#[test]
fn stop_machine_refuses_when_another_session_was_active_and_stops_with_force() {
    let place = Place::new("stop");
    place.add_lab();
    let path = place.notebook("a.jl");
    let mut front = place.front();
    front.initialize();
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    let port = url_port(&used["browser_url"]);
    let opened = other_agent(port, TOKEN, "someone-else", "open_notebook", json!({ "path": path }));
    assert_eq!(opened["notebook_id"], NOTEBOOK, "{opened}");

    let refused = front.ok("stop_machine", json!({ "machine": "lab" }));
    assert_eq!(refused["stopped"], false);
    assert_eq!(refused["active_sessions"], 1);
    assert!(refused["active_seconds_ago"].as_u64().is_some_and(|s| s < 60));
    assert!(refused["message"].as_str().unwrap().contains("another session was active on lab") && refused["message"].as_str().unwrap().contains("force true"), "{refused}");
    assert!(pid_alive(place.runtime().unwrap()), "nothing was stopped");
    assert_eq!(front.ok("list_notebooks", json!({}))[0]["path"], path, "and the session still works");

    let runtime_pid = place.runtime().unwrap();
    let stopped = front.ok("stop_machine", json!({ "machine": "lab", "force": true }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the runtime to end", || !pid_alive(runtime_pid));
    assert!(!place.helpers().is_empty(), "the connection stays");
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains("was stopped from this session") && text(&said).contains("`use_machine`"), "{said}");
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!((status["machine"].as_str(), status["ready"].clone()), (Some("lab"), json!(false)), "{status}");

    // Starting it again works, on the same port.
    let again = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(url_port(&again["browser_url"]), port, "{again}");
    assert_ne!(place.runtime(), Some(runtime_pid));
    // Nothing runs, so a stop says so.
    front.ok("stop_machine", json!({ "machine": "lab", "force": true }));
    let nothing = front.ok("stop_machine", json!({ "machine": "lab" }));
    assert_eq!(nothing["stopped"], false, "{nothing}");
    assert!(nothing["message"].as_str().unwrap().contains("isn't running on lab"), "{nothing}");
}

#[test]
fn stop_machine_works_on_this_computer_with_the_same_check() {
    let place = Place::new("stop-local");
    let path = place.project.join("l.jl");
    std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
    place.local_bridge.set_notebooks(vec![notebook_json(NOTEBOOK, &path.display().to_string())]);
    let mut front = place.front();
    front.initialize();
    front.ok("use_machine", json!({ "machine": "local" }));
    assert_eq!(front.ok("list_notebooks", json!({}))[0]["path"], path.display().to_string());
    let local = place.local_runtime().unwrap();
    // Another session works there.
    let port = std::fs::read_to_string(place.local_state.join("runtime.json")).map(|t| serde_json::from_str::<Value>(&t).unwrap()).unwrap()["port"].as_u64().unwrap() as u16;
    other_agent(port, TOKEN, "someone-else", "open_notebook", json!({ "path": path.display().to_string() }));

    let refused = front.ok("stop_machine", json!({ "machine": "local" }));
    assert_eq!((refused["stopped"].clone(), refused["active_sessions"].clone()), (json!(false), json!(1)), "{refused}");
    assert!(pid_alive(local));
    let stopped = front.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the local runtime to end", || !pid_alive(local));
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains("Julia on this computer was stopped") && text(&said).contains("`use_machine` with machine \"local\""), "{said}");
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!((status["state"].as_str(), status["machine"].as_str()), (Some("stopped"), Some("local")), "{status}");
    assert!(place.local_runtime().is_none() || !pid_alive(place.local_runtime().unwrap()), "not started again by the call");
    let back = front.ok("use_machine", json!({ "machine": "local" }));
    assert_eq!(back["state"], "ready", "{back}");
    assert!(pid_alive(place.local_runtime().unwrap()));
}

#[test]
fn a_session_keeps_its_key_across_machines_and_its_notebook_where_it_made_it() {
    let place = Place::new("sessions");
    place.add_lab();
    let path = place.notebook("a.jl");
    let mut front = place.front();
    front.initialize();
    front.ok("use_machine", json!({ "machine": "lab" }));
    let joined = front.ok("open_notebook", json!({ "path": path }));
    assert_eq!(joined["already_open"], true, "{joined}");
    assert_eq!(front.ok("list_notebooks", json!({}))[0]["this_session"], true);

    // On this computer the session has no notebook; back on the machine it still has the one it made there.
    front.ok("use_machine", json!({ "machine": "local" }));
    front.ok("use_machine", json!({ "machine": "lab" }));
    let listed = front.ok("list_notebooks", json!({}));
    assert_eq!(listed[0]["this_session"], true, "the same key, so the same binding: {listed}");

    // The front's end tells the runtime nothing: it lets go of the machine and leaves the runtime.
    let runtime = place.runtime().unwrap();
    front.finish();
    assert!(pid_alive(runtime));
    wait_for("the connection to end", || place.helpers().is_empty());
}

#[test]
fn a_front_that_ends_leaves_the_runtime_and_a_new_front_attaches_to_it_with_the_notebook_open() {
    let place = Place::new("front-ends");
    place.add_lab();
    let path = place.notebook("kept.jl");
    let mut first = place.front();
    first.initialize();
    let used = first.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((used["state"].as_str(), used["already_running"].clone()), (Some("ready"), json!(false)), "{used}");
    first.ok("open_notebook", json!({ "path": path }));
    let runtime = place.runtime().unwrap();
    first.finish();
    wait_for("the connection to end", || place.helpers().is_empty());
    assert!(pid_alive(runtime), "the runtime on the server runs on");
    assert_eq!(place.runtime(), Some(runtime));

    let mut second = place.front();
    second.initialize();
    let again = second.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((again["state"].as_str(), again["already_running"].clone()), (Some("ready"), json!(true)), "attached, not started: {again}");
    assert!(again["message"].as_str().unwrap().contains("A runtime was already running there"), "{again}");
    assert_eq!(place.runtime(), Some(runtime), "the same runtime");
    let listed = second.ok("list_notebooks", json!({}));
    assert_eq!((listed[0]["path"].as_str(), listed[0]["this_session"].clone()), (Some(path.as_str()), json!(false)), "the notebook is still open there: {listed}");
    second.finish();
}

#[test]
fn a_machines_runtime_of_another_interface_is_kept_and_the_agent_is_told_once_and_one_offering_this_interface_is_not_mentioned() {
    let place = Place::new("machine-other-build");
    place.add_lab();
    let path = place.notebook("kept.jl");
    let mut first = place.front();
    first.initialize();
    first.ok("use_machine", json!({ "machine": "lab" }));
    first.ok("open_notebook", json!({ "path": path }));
    let runtime = place.runtime().unwrap();
    first.finish();
    wait_for("the connection to end", || place.helpers().is_empty());

    // Another build, but this build's interface: used as it is, and nothing is said.
    let mut record = read_record(&place.state);
    assert_eq!(record["interface"], endeavor_mcp::CORE_INTERFACE, "the core records its interface");
    record["build"] = ANOTHER_BUILD.into();
    std::fs::write(place.state.join("runtime.json"), record.to_string()).unwrap();
    let mut second = place.front();
    second.initialize();
    let used = second.ok("use_machine", json!({ "machine": "lab" }));
    assert!(!used["message"].as_str().unwrap().contains("version of Endeavor"), "{used}");
    let (_, said) = second.contents("list_notebooks", json!({}));
    assert_eq!(said.len(), 1, "nothing added: {said:?}");
    let status = second.ok("pluto_session_status", json!({}));
    assert!(status.get("other_version").is_none() && status.get("runtime_build").is_none(), "{status}");
    second.finish();
    wait_for("the connection to end", || place.helpers().is_empty());

    // Another build from before the interface was recorded: kept, and the agent is told once, by use_machine.
    record.as_object_mut().unwrap().remove("interface");
    std::fs::write(place.state.join("runtime.json"), record.to_string()).unwrap();
    let mut third = place.front();
    third.initialize();
    let used = third.ok("use_machine", json!({ "machine": "lab" }));
    let message = used["message"].as_str().unwrap();
    assert!(message.contains("Note: Julia on lab was started by an older version of Endeavor, and it keeps running as it is.") && message.contains("`stop_machine` with machine \"lab\""), "{used}");
    assert!(!message.contains(ANOTHER_BUILD), "no build keys in what the agent passes on: {used}");
    let (_, said) = third.contents("list_notebooks", json!({}));
    assert_eq!(said.len(), 1, "said once: {said:?}");
    // The status says it every time, for an agent that no longer has the notice, with the build in a field of its own.
    let status = third.ok("pluto_session_status", json!({}));
    let other = status["other_version"].as_str().unwrap_or_default();
    assert!(other.starts_with("Julia on lab was started by an older version of Endeavor") && !other.contains(ANOTHER_BUILD), "{status}");
    assert_eq!((status["runtime_build"].as_str(), status["machine"].as_str()), (Some(ANOTHER_BUILD), Some("lab")), "{status}");
    // One whose interface is above this build's is a newer version.
    third.finish();
    wait_for("the connection to end", || place.helpers().is_empty());
    record["interface"] = (endeavor_mcp::CORE_INTERFACE + 1).into();
    std::fs::write(place.state.join("runtime.json"), record.to_string()).unwrap();
    let mut third = place.front();
    third.initialize();
    let used = third.ok("use_machine", json!({ "machine": "lab" }));
    assert!(used["message"].as_str().unwrap().contains("Julia on lab was started by a newer version of Endeavor"), "{used}");
    assert_eq!(place.runtime(), Some(runtime), "kept");
    third.finish();
}

#[test]
fn a_dropped_connection_is_made_again_in_the_same_front_on_the_same_browser_address() {
    let place = Place::new("dropped");
    place.add_lab();
    let path = place.notebook("held.jl");
    let mut front = place.front();
    front.initialize();
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    let (address, runtime) = (used["browser_url"].clone(), place.runtime().unwrap());
    assert_eq!(front.ok("list_notebooks", json!({}))[0]["path"], path);

    let before = place.helpers();
    for pid in &before {
        // SAFETY: plain syscall, on a helper of this test's own front.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    wait_for("the connection to be made again", || {
        let now = place.helpers();
        !now.is_empty() && now.iter().all(|pid| !before.contains(pid))
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        let (failed, status) = front.call("pluto_session_status", json!({}));
        if !failed && status.get("browser_url").is_some() {
            break status;
        }
        assert!(Instant::now() < deadline, "{status}\n{:?}", front.said());
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(status["browser_url"], address, "the address the user has open is the same");
    assert_eq!(place.runtime(), Some(runtime), "attached to the runtime that was running, not a new one");
    assert_eq!(front.ok("list_notebooks", json!({}))[0]["path"], path);
    front.finish();
}

#[test]
fn one_front_uses_two_machines_each_with_its_own_connection_and_runtime() {
    let place = Place::bare("two", &[("ENDEAVOR_TEST_ROOT", "{dir}/root-{id}"), ("ENDEAVOR_TEST_STATE", "{dir}/runtime-state-{id}")]);
    let second_state = place.dir.join("runtime-state-lab2");
    let state_of = |id: &str| place.dir.join(format!("runtime-state-{id}"));
    std::fs::create_dir_all(state_of("lab")).unwrap();
    std::fs::create_dir_all(&second_state).unwrap();
    let bridges = [("lab", FakeBridge::start(&state_of("lab"))), ("lab2", FakeBridge::start(&second_state))];
    for (id, bridge) in &bridges {
        std::fs::write(state_of(id).join("token"), TOKEN).unwrap();
        common::install_helper(&place.dir.join(format!("root-{id}")));
        let julia = serving_julia(&state_of(id), bridge);
        place.machines().save(Server { id: (*id).into(), name: (*id).into(), ssh_host: (*id).into(), julia: Some(julia.display().to_string()), ..Default::default() }).unwrap();
        let path = place.project.join(format!("{id}.jl")).display().to_string();
        std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
        bridge.set_notebooks(vec![notebook_json(NOTEBOOK, &path)]);
    }
    let mut front = place.front();
    front.initialize();

    let first = front.ok("use_machine", json!({ "machine": "lab" }));
    let second = front.ok("use_machine", json!({ "machine": "lab2" }));
    assert_eq!((first["state"].as_str(), second["state"].as_str()), (Some("ready"), Some("ready")), "{first} {second}");
    let (one, two) = (url_port(&first["browser_url"]), url_port(&second["browser_url"]));
    assert_ne!(one, two, "each machine has its own address");
    assert_eq!((first["remote_port"].clone(), second["remote_port"].clone()), (json!(read_json_port(&state_of("lab"))), json!(read_json_port(&second_state))));
    let (r1, r2) = (recorded_pid(&state_of("lab")).unwrap(), recorded_pid(&second_state).unwrap());
    assert_ne!(r1, r2, "two runtimes");

    // The session is on the second; both connections are up, and the first is still in use.
    assert!(front.ok("list_notebooks", json!({}))[0]["path"].as_str().unwrap().ends_with("lab2.jl"));
    let listed = front.ok("list_machines", json!({}));
    assert_eq!((listed["machines"][0]["state"].as_str(), listed["machines"][1]["state"].as_str()), (Some("ready"), Some("ready")), "{listed}");
    let back = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((back["already_running"].clone(), url_port(&back["browser_url"])), (json!(true), one), "the first one's connection was kept: {back}");
    assert!(front.ok("list_notebooks", json!({}))[0]["path"].as_str().unwrap().ends_with("/lab.jl"));
    assert_eq!(url_port(&front.ok("pluto_session_status", json!({}))["browser_url"]), one);
    // Stopping one leaves the other.
    assert_eq!(front.ok("stop_machine", json!({ "machine": "lab2", "force": true }))["stopped"], true);
    wait_for("the second runtime to end", || !pid_alive(r2));
    assert!(pid_alive(r1));
    front.finish();
    assert!(pid_alive(r1), "ending the front leaves the first runtime");
    wait_for("the connections to end", || place.all_helpers().is_empty());
    end_group(r1);
}

/// End a runtime of this test: its core, Julia and workers are one process group.
fn end_group(pid: i32) {
    // SAFETY: plain syscalls, on a core this test started and the processes in its group.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
        libc::kill(pid, libc::SIGTERM);
    }
    wait_for("the runtime to end", || !pid_alive(pid));
}

#[test]
fn list_machines_connects_to_nothing_and_says_so() {
    let place = Place::new("lists");
    place.add_lab();
    place.add_hpc();
    let mut front = place.front();
    front.initialize();
    let listed = front.ok("list_machines", json!({}));
    assert_eq!(listed["machines"].as_array().unwrap().iter().map(|m| m["state"].clone()).collect::<Vec<_>>(), [json!("not connected"), json!("not connected")], "{listed}");
    assert!(listed["message"].as_str().unwrap().contains("shows a state only while this session is connected to it"), "{listed}");
    std::thread::sleep(Duration::from_millis(500));
    assert!(place.helpers().is_empty(), "listing made no connection");
    assert!(place.runtime().is_none());
    // A machine the session is connected to shows its state, and the others still show none.
    front.ok("use_machine", json!({ "machine": "lab" }));
    let listed = front.ok("list_machines", json!({}));
    assert_eq!((listed["machines"][0]["state"].as_str(), listed["machines"][1]["state"].as_str()), (Some("ready"), Some("not connected")), "{listed}");
    front.finish();
}

#[test]
fn the_runtimes_own_port_on_the_server_is_in_the_results() {
    let place = Place::new("remote-port");
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    let port = place.runtime_port();
    assert_eq!(used["remote_port"], port, "{used}");
    assert_ne!(url_port(&used["browser_url"]), port, "the address in the browser is this computer's, not the server's");
    let message = used["message"].as_str().unwrap();
    assert!(message.contains("works while this session is connected") && message.contains(&format!("`ssh -L {port}:127.0.0.1:{port} lab`")), "{message}");
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!(status["remote_port"], port, "{status}");
    front.finish();
}

fn local_notebook(place: &Place, name: &str) -> String {
    let path = place.project.join(name).display().to_string();
    std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
    place.local_bridge.set_notebooks(vec![notebook_json(NOTEBOOK, &path)]);
    path
}

fn place_none(place: &Place) -> bool {
    place.local_runtime().is_none() && core_of(place).is_empty()
}

fn core_of(place: &Place) -> Vec<i32> {
    pids(&format!("core --state-dir {}", place.local_state.display()))
}

#[test]
fn a_front_that_only_initializes_and_lists_tools_starts_no_local_runtime() {
    let place = Place::new("lazy-none");
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    for name in ["list_machines", "notebook_guide", "list_notebooks", "pluto_session_status"] {
        let (failed, said) = front.call(name, json!({}));
        assert!(!failed, "{name}: {said}");
    }
    assert_eq!(front.ok("list_notebooks", json!({})), json!([]));
    assert_eq!(front.ok("pluto_session_status", json!({}))["pluto"], "not running");
    assert_eq!(front.ok("list_machines", json!({}))["local"]["state"], "not running");
    assert!(!place.local_state.join("runtime.json").exists());
    assert!(core_of(&place).is_empty(), "{:?}", core_of(&place));
    front.finish();
}

#[test]
fn the_first_notebook_call_starts_the_local_runtime_and_a_second_front_finds_it() {
    let place = Place::new("lazy-first");
    let path = local_notebook(&place, "first.jl");
    let mut front = place.front();
    front.initialize();
    assert!(place.local_runtime().is_none());
    front.ok("open_notebook", json!({ "path": path }));
    let runtime = place.local_runtime().expect("started by the call");
    assert!(pid_alive(runtime));

    let mut second = place.front();
    second.initialize();
    assert_eq!(second.ok("list_notebooks", json!({}))[0]["path"], path);
    assert!(second.ok("pluto_session_status", json!({})).get("browser_url").is_some());
    assert_eq!(place.local_runtime(), Some(runtime));
    assert_eq!(second.ok("list_machines", json!({}))["local"]["state"], "running");
    second.finish();
    front.finish();
}

#[test]
fn only_a_call_of_a_known_tool_starts_the_local_runtime() {
    let place = Place::new("lazy-known");
    let mut front = place.front();
    front.initialize();
    front.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 } }));
    front.send(json!({ "jsonrpc": "2.0", "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } }));
    for method in ["resources/list", "prompts/list", "logging/setLevel"] {
        let reply = front.request(method, json!({}));
        assert_eq!(reply["error"]["code"], -32601, "{method}: {reply}");
    }
    let (failed, said) = front.call("no_such_tool", json!({}));
    assert!(failed && said["error"] == "unknown_tool" && said["message"].as_str().unwrap().contains("Unknown tool: 'no_such_tool'"), "{said}");
    let reply = front.request("tools/call", json!({ "name": "notebook_guide", "arguments": 5 }));
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("arguments must be an object"), "{reply}");
    let guide = front.request("tools/call", json!({ "name": "notebook_guide", "arguments": {} }));
    assert_eq!(guide["result"]["isError"], false, "{guide}");
    let (failed, said) = front.call("new_notebook", json!({ "name": "remote.jl" }));
    assert!(failed && said["error"] == "invalid_argument" && said["message"].as_str().unwrap().starts_with("`name` is not an argument of `new_notebook`. Its arguments: `path`."), "{said}");
    let (failed, said) = front.call("edit_cell", json!({ "notebook_id": NOTEBOOK, "cell_id": "c" }));
    assert!(failed && said["error"] == "invalid_argument" && said["message"].as_str().unwrap().starts_with("`edit_cell` needs `code`."), "{said}");
    let (failed, said) = front.call("use_machine", json!({}));
    assert!(failed && said["error"] == "invalid_argument" && said["message"] == "`use_machine` needs `machine`.", "{said}");
    let listed = front.request("tools/call", json!({ "name": "list_machines", "arguments": null }));
    assert_eq!(listed["result"]["isError"], false, "null arguments are none: {listed}");
    let (failed, said) = front.call("add_machine", json!({ "host": "lab", "hostname": "x" }));
    let message = said["message"].as_str().unwrap();
    assert!(failed && message.starts_with("`hostname` is not an argument of `add_machine`. Its arguments: ") && message.contains("`host`") && message.contains("`slurm`"), "{said}");
    std::thread::sleep(Duration::from_millis(500));
    assert!(place_none(&place), "{:?}", core_of(&place));
    front.finish();
}

#[test]
fn a_status_query_after_the_runtime_it_used_has_exited_starts_none() {
    let place = Place::new("lazy-exited");
    let path = local_notebook(&place, "gone.jl");
    let mut first = place.front();
    first.initialize();
    first.ok("open_notebook", json!({ "path": path }));
    let runtime = place.local_runtime().expect("started by the call");
    let mut second = place.front();
    second.initialize();
    assert!(second.ok("pluto_session_status", json!({})).get("browser_url").is_some(), "it uses the runtime that is running");
    // SAFETY: plain syscall, on the core this test's front started.
    unsafe { libc::kill(-runtime, libc::SIGTERM) };
    wait_for("the runtime to end", || core_of(&place).is_empty() && !pid_alive(runtime));
    let status = second.ok("pluto_session_status", json!({}));
    assert_eq!(status["pluto"], "not running", "{status}");
    assert_eq!(second.ok("list_notebooks", json!({})), json!([]));
    assert!(core_of(&place).is_empty(), "no runtime was started for it");
    second.finish();
    first.finish();
}

const ANOTHER_BUILD: &str = "0.0.1-0123456789abcdef";

/// The local runtime's record says another build started it, as one that outlived an update would:
/// one from before the interface was recorded.
fn from_another_build(place: &Place) {
    let mut record = read_record(&place.local_state);
    record["build"] = ANOTHER_BUILD.into();
    record.as_object_mut().unwrap().remove("interface");
    std::fs::write(place.local_state.join("runtime.json"), record.to_string()).unwrap();
}

#[test]
fn a_runtime_of_another_build_that_offers_this_builds_interface_is_used_as_it_is() {
    let place = Place::new("other-build-same-interface");
    let path = place.project.join("idle.jl").display().to_string();
    std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
    let mut first = place.front();
    first.initialize();
    first.call("open_notebook", json!({ "path": path }));
    let old = place.local_runtime().expect("started by the call");
    first.finish();
    let mut record = read_record(&place.local_state);
    assert_eq!(record["interface"], endeavor_mcp::CORE_INTERFACE, "the core records its interface");
    record["build"] = ANOTHER_BUILD.into();
    std::fs::write(place.local_state.join("runtime.json"), record.to_string()).unwrap();

    // No notebook is open, and the call may start one: it is still used, and nothing is said.
    let mut second = place.front();
    second.initialize();
    let (failed, said) = second.contents("list_notebooks", json!({}));
    assert!(!failed && said == [json!([])], "nothing added: {said:?}");
    second.call("open_notebook", json!({ "path": path }));
    assert_eq!(place.local_runtime(), Some(old), "kept");
    assert!(!second.said().iter().any(|line| line.contains("was started by")), "{:?}", second.said());
    second.finish();
}

#[test]
fn a_runtime_of_another_build_with_no_notebook_open_is_replaced_by_the_first_call_that_may_start_one() {
    let place = Place::new("other-build-idle");
    let path = place.project.join("idle.jl").display().to_string();
    std::fs::write(&path, "### A Pluto.jl notebook ###").unwrap();
    let mut first = place.front();
    first.initialize();
    // The stand-in has no such notebook to open, but the call starts the runtime.
    first.call("open_notebook", json!({ "path": path }));
    let old = place.local_runtime().expect("started by the call");
    first.finish();
    from_another_build(&place);

    let mut second = place.front();
    second.initialize();
    let (failed, listed) = second.contents("list_notebooks", json!({}));
    assert!(!failed && listed == [json!([])], "a query is answered by the runtime there, with nothing to add: {listed:?}");
    assert_eq!(place.local_runtime(), Some(old), "a query starts nothing, so it stops nothing");
    let status = second.ok("pluto_session_status", json!({}));
    assert!(status.get("other_version").is_none() && status.get("runtime_build").is_none(), "the next start stops it, so the status doesn't say it is kept: {status}");
    second.call("open_notebook", json!({ "path": path }));
    let new = place.local_runtime().expect("a runtime");
    assert!(new != old && !pid_alive(old), "{old} was stopped and {new} started");
    assert_ne!(read_record(&place.local_state)["build"], ANOTHER_BUILD);
    assert!(second.said().iter().any(|line| line.contains(&format!("(pid {old}) was started by build {ANOTHER_BUILD}, and no notebook is open in it"))), "{:?}", second.said());
    second.finish();
}

#[test]
fn a_runtime_of_another_build_with_a_notebook_open_is_kept_and_the_agent_is_told_once() {
    let place = Place::new("other-build-open");
    let path = local_notebook(&place, "kept.jl");
    let mut first = place.front();
    first.initialize();
    first.ok("open_notebook", json!({ "path": path }));
    let runtime = place.local_runtime().expect("started by the call");
    first.finish();
    from_another_build(&place);

    let mut second = place.front();
    second.initialize();
    let (failed, said) = second.contents("list_notebooks", json!({}));
    assert!(!failed && said[0][0]["path"] == path.as_str(), "{said:?}");
    let notice = said[1].as_str().unwrap_or_default();
    assert!(notice.starts_with("Note: Julia on this computer was started by an older version of Endeavor, and it keeps running as it is. It has 1 notebook open. Some notebook tools"), "{notice}");
    assert!(notice.contains("`stop_machine` with machine \"local\"; then `use_machine` with machine \"local\" starts this version."), "{notice}");
    assert!(!notice.contains(ANOTHER_BUILD), "{notice}");
    second.ok("open_notebook", json!({ "path": path }));
    let (_, again) = second.contents("list_notebooks", json!({}));
    assert_eq!(again.len(), 1, "said once: {again:?}");
    let status = second.ok("pluto_session_status", json!({}));
    assert!(status["other_version"].as_str().is_some_and(|t| t.starts_with("Julia on this computer was started by an older version")), "{status}");
    assert_eq!(status["runtime_build"].as_str(), Some(ANOTHER_BUILD), "{status}");
    assert_eq!(place.local_runtime(), Some(runtime), "kept");
    second.finish();
}

#[test]
fn a_front_that_attaches_through_list_notebooks_says_where_the_notebooks_are() {
    let place = Place::new("lazy-attach");
    let path = local_notebook(&place, "attach.jl");
    let mut first = place.front();
    first.initialize();
    first.ok("open_notebook", json!({ "path": path }));
    let mut second = place.front();
    second.initialize();
    assert_eq!(second.ok("list_notebooks", json!({}))[0]["path"], path);
    wait_for("the notebooks line", || second.said().iter().any(|line| line.starts_with("Endeavor's notebooks: http://localhost:")));
    second.finish();
    first.finish();
}

#[test]
fn a_recorded_runtime_that_cannot_be_used_is_an_error_for_the_two_queries_and_starts_none() {
    let place = Place::new("lazy-unusable");
    let mut front = place.front();
    front.initialize();
    let record = |node: &str, pid: u32, port: Option<u16>| {
        let mut record = json!({ "launcher": "process", "node": node, "pid": pid, "token": "t" });
        if let Some(port) = port {
            record["port"] = port.into();
        }
        std::fs::write(place.local_state.join("runtime.json"), record.to_string()).unwrap();
    };
    let mut sleeper = Command::new("sleep").arg("600").spawn().unwrap();
    // The status answers with the state and why; the listing fails with the same words.
    let why = |front: &mut Front, tool: &str| {
        let (failed, said) = front.call(tool, json!({}));
        assert_eq!(failed, tool == "list_notebooks", "{tool}: {said}");
        if failed { text(&said).to_owned() } else { said["message"].as_str().unwrap().to_owned() }
    };
    for tool in ["pluto_session_status", "list_notebooks"] {
        record("another-node", sleeper.id(), Some(1));
        assert!(why(&mut front, tool).contains("running on another-node"), "{tool}");
        record(&this_host(), sleeper.id(), None);
        assert!(why(&mut front, tool).contains("older version"), "{tool}");
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    record(&this_host(), sleeper.id(), Some(port));
    assert!(why(&mut front, "pluto_session_status").contains("isn't answering"));
    assert!(core_of(&place).is_empty(), "no runtime was started");
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    front.finish();
}

#[test]
fn a_notebook_call_waits_for_a_silent_local_runtime_then_says_it_is_stuck_and_starts_none() {
    let place = Place::new("lazy-silent");
    let mut front = start_front(&place, &[("ENDEAVOR_TEST_SILENT_WAIT_SECS", "2")]);
    front.initialize();
    let mut sleeper = Command::new("sleep").arg("600").spawn().unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let record = json!({ "launcher": "process", "node": this_host(), "pid": sleeper.id(), "token": "t", "port": closed });
    std::fs::write(place.local_state.join("runtime.json"), record.to_string()).unwrap();
    let began = Instant::now();
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains(&format!("(pid {}) is running but hasn't answered for 2 seconds, so no second one was started beside it", sleeper.id())), "{said}");
    assert!(began.elapsed() >= Duration::from_secs(2), "{:?}", began.elapsed());
    assert!(core_of(&place).is_empty(), "no runtime was started");
    assert!(sleeper.try_wait().unwrap().is_none(), "not stopped");
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    front.finish();
}

#[test]
fn use_machine_local_starts_the_local_runtime() {
    let place = Place::new("lazy-use-local");
    let mut front = place.front();
    front.initialize();
    assert!(place.local_runtime().is_none());
    assert_eq!(front.ok("use_machine", json!({ "machine": "local" }))["state"], "ready");
    assert!(place.local_runtime().is_some_and(pid_alive));
    front.finish();
}

#[test]
fn a_first_notebook_call_during_a_slow_start_says_to_call_again_and_a_later_call_succeeds() {
    let place = Place::new("lazy-slow");
    let path = local_notebook(&place, "slow.jl");
    std::fs::write(place.local_state.join("hold"), "").unwrap();
    let mut front = start_front(&place, &[("ENDEAVOR_START_WAIT_SECS", "3")]);
    front.initialize();
    let started = Instant::now();
    let (failed, said) = front.call("open_notebook", json!({ "path": path }));
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    assert!(failed && text(&said).contains("Julia is starting on this computer") && text(&said).contains("`pluto_session_status`"), "{said}");
    assert!(place.local_runtime().is_none());
    std::fs::remove_file(place.local_state.join("hold")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (failed, said) = front.call("open_notebook", json!({ "path": path }));
        if !failed {
            break;
        }
        assert!(text(&said).contains("Julia is starting on this computer"), "{said}");
        assert!(Instant::now() < deadline, "{said}");
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(place.local_runtime().is_some_and(pid_alive));
    front.finish();
}

/// Hold `dir/start.lock`, as a process in the middle of a start does.
fn hold_start_lock(dir: &Path) -> std::fs::File {
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("start.lock")).unwrap();
    // The front holds the lock for a moment around each look, so one try can meet it.
    wait_for("the start lock to be free", || {
        // SAFETY: plain syscall on a file we hold open.
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    });
    file
}

#[test]
fn stopping_this_computer_waits_for_a_start_under_way_and_leaves_the_note_of_a_stop_from_a_connection() {
    let place = Place::new("lazy-stop-lock");
    let path = local_notebook(&place, "stop.jl");
    let mut front = start_front(&place, &[("ENDEAVOR_STOP_LOCK_SECS", "1")]);
    front.initialize();
    front.ok("open_notebook", json!({ "path": path }));
    let runtime = place.local_runtime().unwrap();

    let held = hold_start_lock(&place.local_state);
    let (failed, said) = front.call("stop_machine", json!({ "machine": "local", "force": true }));
    assert!(failed && text(&said).contains("Julia was not stopped") && text(&said).contains("start lock"), "{said}");
    assert!(pid_alive(runtime));
    drop(held);
    let stopped = front.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the runtime to end", || !pid_alive(runtime));
    let note = std::fs::read_to_string(place.local_state.join("stopped")).unwrap();
    assert_eq!(note, format!("{runtime} connection"), "other clients hear it was stopped from another connection");
    front.finish();
}

#[test]
fn a_forced_stop_cancels_a_start_another_process_began_and_the_next_call_starts_afresh() {
    let place = Place::new("lazy-cancel");
    let path = local_notebook(&place, "cancelled.jl");
    std::fs::write(place.local_state.join("hold"), "").unwrap();
    let short = [("ENDEAVOR_START_WAIT_SECS", "2")];
    let mut first = start_front(&place, &short);
    first.initialize();
    let (failed, said) = first.call("open_notebook", json!({ "path": path }));
    assert!(failed && text(&said).contains("Julia is starting on this computer"), "{said}");
    let started = core_of(&place);
    let julia = common::julia_pids(&place.local_state);
    assert_eq!((started.len(), julia.len()), (1, 1), "{started:?} {julia:?}");
    first.finish();

    let mut second = start_front(&place, &short);
    second.initialize();
    let status = second.ok("pluto_session_status", json!({}));
    assert_eq!((status["state"].as_str(), status["ready"].clone()), (Some("starting"), json!(false)), "{status}");
    let refused = second.ok("stop_machine", json!({ "machine": "local" }));
    assert_eq!(refused["stopped"], false, "{refused}");
    assert_eq!(core_of(&place), started);

    let stopped = second.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the core and Julia to end", || !pid_alive(started[0]) && !pid_alive(julia[0]));
    assert!(core_of(&place).is_empty() && !place.local_state.join("runtime.json").exists(), "nothing of the start is left");
    wait_for("the lock to be let go", || std::fs::File::open(place.local_state.join("starting.lock")).is_ok_and(|file| file.try_lock_shared().is_ok()));
    assert_eq!(std::fs::read_to_string(place.local_state.join("stopped")).unwrap(), format!("{} connection", started[0]));
    let status = second.ok("pluto_session_status", json!({}));
    assert_eq!(status["state"], "stopped", "the cancelled start is a stop, not a failed start: {status}");
    let again = second.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!(again["stopped"], false, "{again}");
    assert!(again["message"].as_str().unwrap().contains("isn't running"), "{again}");

    // Asked to start again, it starts a new runtime.
    std::fs::remove_file(place.local_state.join("hold")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let used = second.ok("use_machine", json!({ "machine": "local" }));
        if used["ready"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "{used}");
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(common::julia_pids(&place.local_state).len(), 2, "a second core ran after the first was cancelled");
    assert!(place.local_runtime().is_some_and(|pid| pid != started[0] && pid_alive(pid)));
    second.finish();
}

#[test]
fn a_forced_stop_cancels_the_start_this_session_began_and_it_is_not_left_as_a_failure() {
    let place = Place::new("lazy-cancel-own");
    let path = local_notebook(&place, "own.jl");
    std::fs::write(place.local_state.join("hold"), "").unwrap();
    let mut front = start_front(&place, &[("ENDEAVOR_START_WAIT_SECS", "2")]);
    front.initialize();
    let (failed, said) = front.call("open_notebook", json!({ "path": path }));
    assert!(failed && text(&said).contains("Julia is starting on this computer"), "{said}");
    let started = core_of(&place);
    assert_eq!(started.len(), 1, "{started:?}");

    let refused = front.ok("stop_machine", json!({ "machine": "local" }));
    assert_eq!(refused["stopped"], false, "{refused}");
    assert_eq!(core_of(&place), started);
    let stopped = front.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the core to end", || !pid_alive(started[0]));
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!(status["state"], "stopped", "{status}");
    // Read without a call that asks to try again: the start it cancelled is not a failure kept for every call.
    assert!(status.get("error").is_none(), "no failure is kept and told to every call: {status}");
    let used = front.ok("use_machine", json!({ "machine": "local" }));
    assert_ne!(used["state"], "failed", "the start it cancelled is not held against the next one: {used}");
    std::fs::remove_file(place.local_state.join("hold")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while front.ok("use_machine", json!({ "machine": "local" }))["ready"] != true {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(common::julia_pids(&place.local_state).len(), 2);
    front.finish();
}

#[test]
fn a_forced_stop_after_a_failed_start_here_reaches_a_start_another_process_has_under_way() {
    let place = Place::new("lazy-failed-then-other");
    let path = local_notebook(&place, "failed.jl");
    std::fs::write(place.local_state.join("hold"), "").unwrap();
    let short = [("ENDEAVOR_START_WAIT_SECS", "2")];
    let mut front = start_front(&place, &short);
    front.initialize();
    let (failed, said) = front.call("open_notebook", json!({ "path": path }));
    assert!(failed && text(&said).contains("Julia is starting on this computer"), "{said}");
    // Its start fails: the core is killed under it.
    let first = core_of(&place);
    assert_eq!(first.len(), 1, "{first:?}");
    // SAFETY: plain syscall, on the core this test's front started and what it started.
    unsafe { libc::kill(-first[0], libc::SIGKILL) };
    wait_for("the failure", || {
        let (failed, said) = front.call("open_notebook", json!({ "path": path }));
        failed && text(&said).contains("stopped while starting")
    });
    // Another process begins a start of its own.
    let mut other = start_front(&place, &short);
    other.initialize();
    let (failed, said) = other.call("open_notebook", json!({ "path": path }));
    assert!(failed && text(&said).contains("Julia is starting on this computer"), "{said}");
    let second = core_of(&place);
    assert_eq!(second.len(), 1, "{second:?}");
    assert_ne!(second, first);
    other.finish();

    let stopped = front.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!(stopped["stopped"], true, "the first forced stop reaches the start: {stopped}");
    wait_for("the other process's core to end", || core_of(&place).is_empty());
    front.finish();
}

#[test]
fn a_start_goes_on_when_the_front_that_asked_for_it_has_gone_and_the_next_front_attaches_to_it() {
    let place = Place::new("lazy-abandoned");
    let path = local_notebook(&place, "abandoned.jl");
    std::fs::write(place.local_state.join("hold"), "").unwrap();
    let short = [("ENDEAVOR_START_WAIT_SECS", "2")];
    let mut first = start_front(&place, &short);
    first.initialize();
    let (failed, said) = first.call("open_notebook", json!({ "path": path }));
    assert!(failed && text(&said).contains("Julia is starting on this computer"), "{said}");
    let started = core_of(&place);
    assert_eq!(started.len(), 1, "{started:?}");
    first.finish();
    assert!(pid_alive(started[0]) && place.local_runtime().is_none(), "the start goes on without the front");

    // A front that comes while the start is still under way waits for it and starts none.
    let mut second = start_front(&place, &short);
    second.initialize();
    let status = second.ok("pluto_session_status", json!({}));
    assert_eq!((status["state"].as_str(), status["ready"].clone()), (Some("starting"), json!(false)), "another process's start is a start: {status}");
    let refused = second.ok("stop_machine", json!({ "machine": "local" }));
    let said = refused["message"].as_str().unwrap();
    assert!(refused["stopped"] == false && refused["state"] == "starting" && said.contains("Julia is starting on this computer") && said.contains("Stopping cancels it") && said.contains("force true"), "{refused}");
    assert_eq!(core_of(&place), started, "a stop without force leaves another process's start alone");
    let (failed, said) = second.call("open_notebook", json!({ "path": path }));
    assert!(failed && text(&said).contains("Julia is starting on this computer"), "{said}");
    assert_eq!(core_of(&place), started, "no second runtime was started");
    std::fs::remove_file(place.local_state.join("hold")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (failed, said) = second.call("open_notebook", json!({ "path": path }));
        if !failed {
            break;
        }
        assert!(text(&said).contains("Julia is starting on this computer"), "{said}");
        assert!(Instant::now() < deadline, "{said}");
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(place.local_runtime(), Some(started[0]), "the second front uses the runtime the first one began");
    assert_eq!(core_of(&place), started);
    second.finish();
}

#[test]
fn a_cluster_gets_no_job_without_resources_then_queues_runs_and_stops() {
    let slurm = FakeSlurm::new("cluster");
    let place = Place::with("cluster", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_hpc();
    let mut front = place.front();
    front.initialize();

    // No job and no resources: nothing is submitted, and the defaults come back to be confirmed.
    let asked = front.ok("use_machine", json!({ "machine": "hpc" }));
    assert_eq!((asked["state"].as_str(), asked["needs_job"].clone(), asked["ready"].clone()), (Some("needs_job"), json!(true), json!(false)), "{asked}");
    assert_eq!((asked["defaults"]["cpus"].clone(), asked["defaults"]["memory_gb"].clone(), asked["defaults"]["hours"].clone()), (json!(8), json!(32), json!(8.0)));
    assert!(asked["message"].as_str().unwrap().contains("nothing was submitted"), "{asked}");
    assert_eq!(slurm.read("sbatch.args"), "", "no job was submitted");
    assert!(asked["message"].as_str().unwrap().contains("this session has not moved: it stays on this computer until `use_machine` is called"), "{asked}");
    assert_eq!(place.projects(), Value::Null, "needs_job writes no project");
    assert_eq!(front.ok("list_notebooks", json!({})), json!([]), "the session is still on this computer");
    assert_eq!(front.ok("pluto_session_status", json!({})).get("machine"), None);
    assert_eq!(slurm.read("sbatch.args"), "", "a notebook call submits nothing either");
    let (failed, plain) = front.call("use_machine", json!({ "machine": "lab", "cpus": 4 }));
    assert!(failed && plain["message"].as_str().unwrap().contains("There is no machine"), "{plain}");

    // With resources: the job is submitted, and the queue shows through pluto_session_status.
    let queued = front.ok("use_machine", json!({ "machine": "hpc", "cpus": 4, "memory_gb": 16, "hours": 2, "partition": "shared", "account": "lab" }));
    assert_eq!((queued["state"].as_str(), queued["ready"].clone()), (Some("queued"), json!(false)), "{queued}");
    assert_eq!((queued["job"]["id"].as_str(), queued["queue"]["state"].as_str(), queued["queue"]["reason"].as_str()), (Some("42"), Some("PENDING"), Some("Priority")), "{queued}");
    assert!(queued["message"].as_str().unwrap().contains("waiting in the queue: other jobs are ahead of it"), "{queued}");
    let sbatch = slurm.read("sbatch.args");
    assert!(sbatch.contains("--account=lab --partition=shared --cpus-per-task=4 --mem=16G --time=120"), "{sbatch}");
    let saved = place.machines().find_by_name("hpc").unwrap().unwrap().cluster.unwrap();
    assert_eq!((saved.resources.cpus, saved.resources.mem_gb, saved.resources.minutes, saved.account.as_deref()), (4, 16, 120, Some("lab")), "what was used is the new default");

    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!((status["machine"].as_str(), status["state"].as_str(), status["ready"].clone()), (Some("hpc"), Some("queued"), json!(false)), "{status}");
    assert_eq!((status["queue"]["state"].as_str(), status["queue"]["reason_text"].as_str(), status["job"]["id"].as_str()), (Some("PENDING"), Some("other jobs are ahead of it"), Some("42")), "{status}");
    slurm.set("reason", "Resources");
    wait_for("the queue's new reason", || front.ok("pluto_session_status", json!({}))["queue"]["reason"] == "Resources");
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains("Slurm job 42 on hpc is waiting in the queue"), "{said}");

    // The job runs: its node starts the runtime, and the session gets to it.
    slurm.set("node", &this_host());
    slurm.set("left", "29:30");
    slurm.set("state", "RUNNING");
    let mut node = Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .args(["node-start", "--state-dir"])
        .arg(&place.state)
        .arg("--julia")
        .arg(&place.julia)
        .args(["--runtime", "/nonexistent", "--depot", "/nonexistent", "--exit-idle"])
        .env("FAKE_JOB", "42")
        .current_dir(&place.dir)
        .process_group(0)
        .spawn()
        .unwrap();
    slurm.set("node.pid", &node.id().to_string());
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let status = front.ok("pluto_session_status", json!({}));
        if status.get("browser_url").is_some() {
            break status;
        }
        assert!(Instant::now() < deadline, "{status}\n{:?}", front.said());
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(status["machine"], "hpc");
    assert_eq!(status["job"]["id"], "42", "{status}");
    assert!(status["job"]["ends_in_minutes"].as_u64().is_some_and(|m| (28..=30).contains(&m)), "{status}");
    assert_eq!(status["job"]["node"], this_host());
    assert_eq!(front.ok("list_notebooks", json!({})), json!([]));
    assert_eq!(slurm.read("sbatch.args").lines().count(), 1, "one job");

    // A later session finds the job running, and takes it with no use_machine and no new job.
    front.finish();
    let mut second = place.front();
    second.initialize();
    assert_eq!(second.ok("pluto_session_status", json!({}))["job"]["id"], "42");
    let used = second.ok("use_machine", json!({ "machine": "hpc" }));
    assert_eq!((used["state"].as_str(), used["already_running"].clone(), used["job"]["id"].as_str()), (Some("ready"), json!(true), Some("42")), "{used}");
    assert!(used["message"].as_str().unwrap().contains("The job ends in"), "{used}");

    // Stopping cancels the job. The first session only asked for the status, so it holds nothing up.
    let stopped = second.ok("stop_machine", json!({ "machine": "hpc" }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    assert_eq!(slurm.read("scancel.log").trim(), "42");
    assert!(stopped["message"].as_str().unwrap().contains("Slurm job was cancelled"));
    let _ = node.kill();
    let _ = node.wait();
}

#[test]
fn a_remembered_cluster_with_no_job_is_not_submitted_for_but_told_what_to_ask() {
    let slurm = FakeSlurm::new("cluster-remembered");
    let place = Place::with("cluster-remembered", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_hpc();
    let mut first = place.front();
    first.initialize();
    first.ok("use_machine", json!({ "machine": "hpc" }));
    assert_eq!(place.projects(), Value::Null, "not remembered while no job was asked for");
    first.finish();
    // A project that remembers the cluster from when a job ran there.
    let remembered = json!({ place.project.display().to_string(): { "machine": "hpc", "folder": null } });
    std::fs::create_dir_all(place.dir.join("state-home/endeavor")).unwrap();
    std::fs::write(place.dir.join("state-home/endeavor/projects.json"), remembered.to_string()).unwrap();

    let mut second = place.front();
    second.initialize();
    let (failed, said) = second.call("list_notebooks", json!({}));
    let message = text(&said);
    assert!(failed && message.contains("This project uses hpc, a Slurm cluster, and no job is running there") && message.contains("The defaults would be 8 CPUs · 32 GB · 8 h on the cluster's default partition") && message.contains("`use_machine`"), "{said}");
    assert_eq!(slurm.read("sbatch.args"), "", "nothing was submitted");
    let status = second.ok("pluto_session_status", json!({}));
    assert_eq!((status["machine"].as_str(), status["state"].as_str()), (Some("hpc"), Some("connected")), "{status}");
}

#[test]
fn a_queued_job_is_cancelled_only_when_the_user_agreed() {
    let slurm = FakeSlurm::new("cluster-queued-stop");
    let place = Place::with("cluster-queued-stop", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_hpc();
    let mut front = place.front();
    front.initialize();
    let queued = front.ok("use_machine", json!({ "machine": "hpc", "cpus": 4 }));
    assert_eq!(queued["state"], "queued", "{queued}");

    let refused = front.ok("stop_machine", json!({ "machine": "hpc" }));
    assert_eq!((refused["stopped"].clone(), refused["job"]["id"].clone(), refused["queue"]["state"].clone()), (json!(false), json!("42"), json!("PENDING")), "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(message.contains("the Slurm job 42 on hpc is pending (other jobs are ahead of it)") && message.contains("can't see which other sessions are waiting") && message.contains("force true"), "{message}");
    assert_eq!(slurm.read("scancel.log"), "", "nothing was cancelled");
    assert_eq!(front.ok("pluto_session_status", json!({}))["state"], "queued");

    let stopped = front.ok("stop_machine", json!({ "machine": "hpc", "force": true }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    assert_eq!(slurm.read("scancel.log").trim(), "42");
}

/// Slurm's commands as scripts over files in `target/tmp/machines-NAME-slurm`: the test moves a job
/// through the queue by writing its state.
struct FakeSlurm {
    dir: PathBuf,
    bin: PathBuf,
}

impl FakeSlurm {
    fn new(name: &str) -> FakeSlurm {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("machines-{name}-slurm"));
        let _ = std::fs::remove_dir_all(&root);
        let (bin, dir) = (root.join("bin"), root.join("state"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let scripts = [
            ("sbatch", "echo \"$@\" >> \"$FAKE_SLURM/sbatch.args\"\nfor a; do last=$a; done\ncp \"$last\" \"$FAKE_SLURM/job.sh\"\necho PENDING > \"$FAKE_SLURM/state\"\necho Priority > \"$FAKE_SLURM/reason\"\necho 42\n"),
            (
                "squeue",
                "for a; do [ \"$p\" = -j ] && job=$a; p=$a; done\nstate=$(cat \"$FAKE_SLURM/state\" 2>/dev/null)\ncase \"$*\" in *\"-t all\"*) echo \"$state\"; exit 0;; esac\ncase \"$state\" in PENDING|RUNNING) ;; *) exit 0;; esac\necho \"$state|$(cat \"$FAKE_SLURM/reason\")|$(cat \"$FAKE_SLURM/node\" 2>/dev/null)|$(cat \"$FAKE_SLURM/left\" 2>/dev/null || echo 8:00:00)\"\n",
            ),
            (
                "scancel",
                // Slurm ends the job's processes: the one the test started as its node.
                "echo \"$@\" >> \"$FAKE_SLURM/scancel.log\"\necho CANCELLED > \"$FAKE_SLURM/state\"\nif [ -f \"$FAKE_SLURM/node.pid\" ]; then p=$(cat \"$FAKE_SLURM/node.pid\"); kill -TERM -- \"-$p\" 2>/dev/null; kill -TERM \"$p\" 2>/dev/null; fi\nexit 0\n",
            ),
            ("sacct", "cat \"$FAKE_SLURM/sacct\" 2>/dev/null\nexit 0\n"),
            (
                "srun",
                "echo \"$@\" >> \"$FAKE_SLURM/srun.args\"\ncase \" $* \" in *\" --unbuffered \"*) u=1;; esac\nwhile [ $# -gt 0 ]; do case \"$1\" in -*) shift;; *) break;; esac; done\n[ -n \"$u\" ] && exec \"$@\"\n\"$@\" > \"$FAKE_SLURM/srun.out\"\ncat \"$FAKE_SLURM/srun.out\"\n",
            ),
            ("sinfo", "echo 'shared*|8:00:00|10|7492'\n"),
        ];
        for (name, body) in scripts {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        FakeSlurm { dir, bin }
    }

    /// A PATH with these commands first.
    fn path(&self) -> String {
        format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap())
    }

    fn set(&self, file: &str, value: &str) {
        std::fs::write(self.dir.join(file), format!("{value}\n")).unwrap();
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.dir.join(file)).unwrap_or_default()
    }
}

/// A stand-in process that is ended when it is dropped, whatever a test did before.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn add_machine_only_looks_until_told_to_install() {
    let place = Place::bare("needs-install", &[]);
    // A runtime that a helper of an earlier install left: the look reports it.
    let core = KillOnDrop(common::core_like_child());
    let core_pid = core.0.id();
    std::fs::write(place.state.join("runtime.json"), json!({ "launcher": "process", "node": "n", "pid": core_pid, "token": "t" }).to_string()).unwrap();
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let first = front.ok("add_machine", json!({ "host": "lab", "julia": julia }));
    assert_eq!((first["state"].as_str(), first["needs_install"].clone(), first["saved"].clone()), (Some("needs_install"), json!(true), json!(false)), "{first}");
    assert_eq!(first["install"]["items"][0]["kind"], "helper", "{first}");
    assert_eq!(first["install"]["running"], json!({ "process": core_pid }), "{first}");
    let message = first["message"].as_str().unwrap();
    assert!(message.contains("`install: true`") && message.contains("Ask the user") && message.contains("MB") && message.contains("already running"), "{message}");
    assert!(message.contains(&place.dir.join("root").display().to_string()), "where it would go: {message}");
    assert!(!place.dir.join("root").exists(), "nothing was installed");
    assert!(place.helpers().is_empty());
    // Nothing is saved: the second call has the same arguments.
    assert!(!place.machines().path().exists());
    assert_eq!(front.ok("list_machines", json!({}))["machines"], json!([]));
    let (failed, said) = front.call("use_machine", json!({ "machine": "lab" }));
    assert!(failed && said["error"] == "machine_not_found", "{said}");
    assert!(!place.dir.join("root").exists());

    // Asking again without the agreement looks again and changes nothing.
    let again = front.ok("add_machine", json!({ "host": "lab", "julia": julia }));
    assert_eq!(again["state"], "needs_install", "{again}");
    assert!(!place.dir.join("root").exists());

    // With the agreement it installs and adds the machine.
    std::fs::remove_file(place.state.join("runtime.json")).unwrap();
    drop(core);
    let done = front.ok("add_machine", json!({ "host": "lab", "julia": julia, "install": true }));
    assert_eq!((done["state"].as_str(), done["saved"].clone()), (Some("connected"), json!(true)), "{done}");
    assert!(place.dir.join("root").join(endeavor_mcp::embedded::BUILD_VERSION).join("endeavor").exists());
    assert_eq!(place.machines().load().unwrap().iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["lab"]);
    assert_eq!(front.ok("list_machines", json!({}))["machines"][0]["name"], "lab");
    let (failed, said) = front.call("add_machine", json!({ "host": "lab", "install": "yes" }));
    assert!(failed && text(&said).contains("install must be true or false"), "{said}");
}

#[test]
fn use_machine_installs_the_helper_only_when_told_to() {
    let place = Place::bare("use-install", &[]);
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    let first = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((first["state"].as_str(), first["ready"].clone(), first["needs_install"].clone()), (Some("needs_install"), json!(false), json!(true)), "{first}");
    assert!(text(&first).contains("`install: true`"), "{first}");
    assert!(!place.dir.join("root").exists(), "nothing was installed");
    assert_eq!(place.projects(), Value::Null, "the project doesn't remember it");
    assert_eq!(front.ok("pluto_session_status", json!({})).get("machine"), None, "the session stays where it was");
    assert_eq!(front.ok("list_machines", json!({}))["machines"][0]["state"], "needs_install");

    let used = front.ok("use_machine", json!({ "machine": "lab", "install": true }));
    assert_eq!(used["state"], "ready", "{used}");
    assert!(place.dir.join("root").join(endeavor_mcp::embedded::BUILD_VERSION).join("endeavor").exists());
    assert_eq!(place.projects()[place.project.display().to_string()]["machine"], "lab");
}

#[test]
fn a_helper_of_an_older_build_is_an_update_that_needs_the_user_too() {
    let place = Place::new("older-helper");
    place.add_lab();
    let root = place.dir.join("root");
    std::fs::rename(root.join(endeavor_mcp::embedded::BUILD_VERSION), root.join("0.0.1-old")).unwrap();
    let mut front = place.front();
    front.initialize();
    let first = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(first["state"], "needs_install", "{first}");
    assert_eq!(first["install"]["update"], true, "{first}");
    assert!(text(&first).contains("this is an update"), "{first}");
    assert!(!root.join(endeavor_mcp::embedded::BUILD_VERSION).exists());
    let used = front.ok("use_machine", json!({ "machine": "lab", "install": true }));
    assert_eq!(used["state"], "ready", "{used}");
    assert!(root.join("0.0.1-old").exists(), "the older one stays");
}

#[test]
fn a_remembered_project_never_installs() {
    let place = Place::bare("remember-install", &[]);
    place.add_lab();
    std::fs::create_dir_all(place.dir.join("state-home/endeavor")).unwrap();
    let remembered = json!({ place.project.display().to_string(): { "machine": "lab", "folder": null } });
    std::fs::write(place.dir.join("state-home/endeavor/projects.json"), remembered.to_string()).unwrap();
    let mut front = place.front();
    front.initialize();
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains("needs to install Endeavor's helper") && text(&said).contains("`install: true`") && text(&said).contains("Ask the user"), "{said}");
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!((status["state"].as_str(), status["ready"].clone()), (Some("needs_install"), json!(false)), "{status}");
    assert!(!place.dir.join("root").exists(), "nothing was installed");
    front.finish();
}

/// A `PATH` of fake `curl` and `wget` that only note they were run (and fail), then the system's own.
fn no_julia_path(place_dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = place_dir.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["curl", "wget"] {
        let script = format!("#!/bin/sh\necho {tool} >> '{}'\necho 'no network in this test' >&2\nexit 1\n", place_dir.join("download-tried").display());
        std::fs::write(bin.join(tool), script).unwrap();
        std::fs::set_permissions(bin.join(tool), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{}:/usr/bin:/bin", bin.display())
}

#[test]
fn julia_is_downloaded_on_the_machine_only_when_the_user_agreed() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("machines-julia-download");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let place = Place::with("julia-download", &[("PATH", &no_julia_path(&dir)), ("SHELL", "/bin/sh")]);
    let tried = place.dir.join("download-tried");
    let _ = std::fs::remove_file(&tried);
    // A server that Julia is not on, as far as a login shell can tell.
    let find = Command::new("/bin/sh").args(["-lc", "command -v julia"]).env("HOME", place.dir.join("home")).env("PATH", no_julia_path(&place.dir)).output().unwrap();
    if find.status.success() {
        eprintln!("skipped: a login shell finds julia at {}", String::from_utf8_lossy(&find.stdout).trim());
        return;
    }
    // No `julia` in the machine's record, so Endeavor looks for it itself.
    place.machines().save(Server { id: "lab".into(), name: "lab".into(), ssh_host: "lab".into(), ..Default::default() }).unwrap();
    let mut front = place.front();
    front.initialize();
    let first = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((first["state"].as_str(), first["install"]["items"][0]["kind"].as_str()), (Some("needs_install"), Some("runtime")), "{first}");
    let message = text(&first);
    assert!(message.contains("wasn't found on lab") && message.contains("download its own copy") && message.contains("MB") && message.contains("`julia`") && message.contains("`install: true` for this call only"), "{message}");
    assert!(!tried.exists(), "nothing was downloaded");
    assert_eq!(place.projects(), Value::Null);
    assert_eq!(front.ok("pluto_session_status", json!({})).get("machine"), None, "the session stays where it was");

    // With the agreement the download is tried (the fake curl fails), on the same connection.
    let (failed, said) = front.call("use_machine", json!({ "machine": "lab", "install": true }));
    assert!(failed && text(&said).contains("Couldn't download Julia"), "{said}");
    assert!(tried.exists(), "the download was tried");
    front.finish();
}

#[test]
fn add_machine_with_install_connects_once_and_without_it_does_not_install() {
    let place = Place::bare("install-once", &[("ENDEAVOR_TEST_ASK", "echo connect >> {dir}/connects")]);
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let done = front.ok("add_machine", json!({ "host": "lab", "julia": julia, "install": true }));
    assert_eq!(done["state"], "connected", "{done}");
    let connects = std::fs::read_to_string(place.dir.join("connects")).unwrap();
    assert_eq!(connects.lines().count(), 1, "one connection, allowed from the start: {connects:?}\n{:?}", front.said());
}

#[test]
fn the_agreement_to_the_helper_from_add_machine_is_not_one_to_install_what_a_start_needs() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("machines-julia-per-start");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let place = Place::bare("julia-per-start", &[("PATH", &no_julia_path(&dir)), ("SHELL", "/bin/sh")]);
    let tried = place.dir.join("download-tried");
    let _ = std::fs::remove_file(&tried);
    let find = Command::new("/bin/sh").args(["-lc", "command -v julia"]).env("HOME", place.dir.join("home")).env("PATH", no_julia_path(&place.dir)).output().unwrap();
    if find.status.success() {
        eprintln!("skipped: a login shell finds julia at {}", String::from_utf8_lossy(&find.stdout).trim());
        return;
    }
    let mut front = place.front();
    front.initialize();
    // The helper is agreed to through add_machine, which says nothing of Julia.
    let added = front.ok("add_machine", json!({ "host": "lab", "install": true, "slurm": false }));
    assert_eq!(added["state"], "connected", "{added}");
    assert!(place.dir.join("root").join(endeavor_mcp::embedded::BUILD_VERSION).join("endeavor").exists());
    let tries = || std::fs::read_to_string(&tried).unwrap_or_default().lines().count();

    // A use_machine without `install` doesn't download Julia, though the helper was agreed to.
    let first = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((first["state"].as_str(), first["install"]["items"][0]["kind"].as_str()), (Some("needs_install"), Some("runtime")), "{first}");
    let message = text(&first);
    assert!(message.contains("wasn't found on lab") && message.contains("download its own copy") && message.contains("MB") && message.contains("`install: true` for this call only"), "{message}");
    assert_eq!(tries(), 0, "nothing was downloaded");
    assert_eq!(place.projects(), Value::Null, "the project doesn't remember it");

    // With `install: true` the download is taken (the fake curl fails).
    let (failed, said) = front.call("use_machine", json!({ "machine": "lab", "install": true }));
    assert!(failed && text(&said).contains("Couldn't download Julia"), "{said}");
    assert_eq!(tries(), 1, "the download was tried");

    // That agreement was for that call: the next one without it asks again and downloads nothing.
    let again = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!((again["state"].as_str(), again["install"]["items"][0]["kind"].as_str()), (Some("needs_install"), Some("runtime")), "{again}");
    assert_eq!(tries(), 1, "no second try");

    // A project that remembers the machine doesn't download either.
    std::fs::create_dir_all(place.dir.join("state-home/endeavor")).unwrap();
    let remembered = json!({ place.project.display().to_string(): { "machine": "lab", "folder": null } });
    std::fs::write(place.dir.join("state-home/endeavor/projects.json"), remembered.to_string()).unwrap();
    let mut second = place.front();
    second.initialize();
    assert_eq!(second.ok("list_notebooks", json!({})), json!([]), "a query starts nothing");
    let (failed, said) = second.call("open_notebook", json!({ "path": "a.jl" }));
    assert!(failed && text(&said).contains("wasn't found on lab"), "{said}");
    assert_eq!(tries(), 1, "a remembered project's call downloads nothing");
    second.finish();
    front.finish();
}

#[test]
fn one_yes_to_use_machine_covers_the_helper_and_the_runtime_in_one_start() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("machines-one-yes");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let place = Place::bare("one-yes", &[("PATH", &no_julia_path(&dir)), ("SHELL", "/bin/sh")]);
    let tried = place.dir.join("download-tried");
    let _ = std::fs::remove_file(&tried);
    let find = Command::new("/bin/sh").args(["-lc", "command -v julia"]).env("HOME", place.dir.join("home")).env("PATH", no_julia_path(&place.dir)).output().unwrap();
    if find.status.success() {
        eprintln!("skipped: a login shell finds julia at {}", String::from_utf8_lossy(&find.stdout).trim());
        return;
    }
    place.machines().save(Server { id: "lab".into(), name: "lab".into(), ssh_host: "lab".into(), ..Default::default() }).unwrap();
    let mut front = place.front();
    front.initialize();
    // Neither is on the machine: the question names the helper, and says the yes goes on to what the start needs.
    let first = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(first["install"]["items"].as_array().map(|items| items.iter().map(|i| i["kind"].clone()).collect::<Vec<_>>()), Some(vec![json!("helper")]), "{first}");
    assert!(text(&first).contains("The same yes covers what this start needs after it"), "{first}");
    assert!(!place.dir.join("root").exists() && !tried.exists(), "nothing was installed");

    // One call with `install: true` installs the helper and goes on to the runtime's install (the fake curl fails).
    let (failed, said) = front.call("use_machine", json!({ "machine": "lab", "install": true }));
    assert!(place.dir.join("root").join(endeavor_mcp::embedded::BUILD_VERSION).join("endeavor").exists(), "the helper was installed");
    assert!(failed && text(&said).contains("Couldn't download Julia"), "{said}");
    assert_eq!(std::fs::read_to_string(&tried).unwrap_or_default().lines().count(), 1, "the download was tried in the same call");
    front.finish();
}

#[test]
fn stop_machine_asks_before_installing_the_helper_it_needs() {
    let place = Place::new("stop-install");
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    let used = front.ok("use_machine", json!({ "machine": "lab" }));
    assert_eq!(used["state"], "ready", "{used}");
    let runtime = place.runtime().expect("a runtime");
    // The front ends, the runtime goes on, and the machine has only a helper of an older build.
    front.finish();
    wait_for("the helper to end", || place.helpers().is_empty());
    let root = place.dir.join("root");
    std::fs::rename(root.join(endeavor_mcp::embedded::BUILD_VERSION), root.join("0.0.1-old")).unwrap();
    std::fs::create_dir_all(root.join("0.0.1-old/runtime")).unwrap();
    std::fs::write(root.join("0.0.1-old/runtime/boot.jl"), "").unwrap();
    let mut front = place.front();
    front.initialize();

    let first = front.ok("stop_machine", json!({ "machine": "lab", "force": true }));
    assert_eq!((first["state"].as_str(), first["stopped"].clone(), first["install"]["items"][0]["kind"].as_str()), (Some("needs_install"), json!(false), Some("helper")), "{first}");
    let message = text(&first);
    assert!(message.contains("Stopping the runtime there needs it") && message.contains("`stop_machine` again") && message.contains("`install: true`"), "{message}");
    assert_eq!(place.runtime(), Some(runtime), "the runtime is still there");
    assert!(!root.join(endeavor_mcp::embedded::BUILD_VERSION).exists(), "nothing was installed");

    let stopped = front.ok("stop_machine", json!({ "machine": "lab", "force": true, "install": true }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the runtime to end", || place.runtime().is_none_or(|pid| !pid_alive(pid)));
}

#[test]
fn no_partitions_are_waited_for_when_the_machine_is_saved_as_a_plain_server() {
    let slurm = FakeSlurm::new("plain-no-wait");
    std::fs::write(slurm.bin.join("sinfo"), "#!/bin/sh\nsleep 8\necho 'shared*|8:00:00|10|7492'\n").unwrap();
    let place = Place::with("plain-no-wait", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let started = Instant::now();
    let added = front.ok("add_machine", json!({ "host": "lab", "julia": julia, "slurm": false }));
    assert_eq!((added["state"].as_str(), added["cluster"].clone()), (Some("connected"), json!(false)), "{added}");
    assert!(started.elapsed() < Duration::from_secs(6), "it didn't wait for sinfo: {:?}", started.elapsed());
}

#[test]
fn a_remembered_plain_server_that_became_a_cluster_gets_no_job_without_the_users_agreement() {
    let slurm = FakeSlurm::new("became-cluster");
    let place = Place::with("became-cluster", &[("PATH", &slurm.path()), ("FAKE_SLURM", &slurm.dir.display().to_string())]);
    place.add_lab();
    let remembered = json!({ place.project.display().to_string(): { "machine": "lab", "folder": null } });
    std::fs::create_dir_all(place.dir.join("state-home/endeavor")).unwrap();
    std::fs::write(place.dir.join("state-home/endeavor/projects.json"), remembered.to_string()).unwrap();
    let mut front = place.front();
    front.initialize();
    assert_eq!(front.ok("list_notebooks", json!({})), json!([]), "connected as a plain server, nothing runs");
    let added = front.ok("add_machine", json!({ "host": "lab", "slurm": true, "julia": place.julia.display().to_string() }));
    assert_eq!(added["cluster"], true, "{added}");
    let path = place.notebook("a.jl");
    let (failed, said) = front.call("open_notebook", json!({ "path": path }));
    assert!(failed && text(&said).contains("a Slurm cluster, and no job is running there"), "{said}");
    assert_eq!(slurm.read("sbatch.args"), "", "nothing was submitted");
    assert!(place.runtime().is_none());
    front.finish();
}

#[test]
fn a_runtime_in_use_through_settings_that_changed_can_still_be_stopped_from_this_session() {
    let place = Place::new("stop-old-settings");
    place.add_lab();
    let mut front = place.front();
    front.initialize();
    assert_eq!(front.ok("use_machine", json!({ "machine": "lab" }))["state"], "ready");
    let runtime = place.runtime().unwrap();
    // The same Julia by another path: other settings, as far as the connection goes.
    let link = place.dir.join("julia-link");
    std::os::unix::fs::symlink(&place.julia, &link).unwrap();
    let mut record = place.machines().find_by_name("lab").unwrap().unwrap();
    record.julia = Some(link.display().to_string());
    place.machines().save(record).unwrap();
    let (failed, said) = front.call("use_machine", json!({ "machine": "lab" }));
    assert!(failed && text(&said).contains("changed while Julia is in use") && text(&said).contains("stop_machine"), "{said}");
    assert!(pid_alive(runtime));
    let stopped = front.ok("stop_machine", json!({ "machine": "lab" }));
    assert_eq!(stopped["stopped"], true, "{stopped}");
    wait_for("the runtime to end", || !pid_alive(runtime));
    front.finish();
}

#[test]
fn a_stop_that_takes_longer_than_the_call_is_marked_only_when_it_succeeds_and_undone_when_it_fails() {
    let place = Place::new("stop-slow");
    let path = local_notebook(&place, "slow.jl");
    let mut front = start_front(&place, &[("ENDEAVOR_START_WAIT_SECS", "2"), ("ENDEAVOR_STOP_LOCK_SECS", "4")]);
    front.initialize();
    front.ok("open_notebook", json!({ "path": path }));
    let runtime = place.local_runtime().unwrap();

    // The stop waits for the start lock longer than it will, and fails after the call has answered.
    let held = hold_start_lock(&place.local_state);
    let slow = front.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!((slow["stopped"].clone(), slow["message"].as_str().unwrap().contains("taking a while")), (json!(false), true), "{slow}");
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains("was stopped from this session"), "while it goes on: {said}");
    wait_for("the stop to give up", || front.call("list_notebooks", json!({})).0 == false);
    assert!(pid_alive(runtime), "Julia runs, and the session can use it again");
    drop(held);

    // The stop gets the lock after the call has answered, and succeeds.
    let held = hold_start_lock(&place.local_state);
    let slow = front.ok("stop_machine", json!({ "machine": "local", "force": true }));
    assert_eq!(slow["stopped"], false, "{slow}");
    drop(held);
    wait_for("the runtime to end", || !pid_alive(runtime));
    let (failed, said) = front.call("list_notebooks", json!({}));
    assert!(failed && text(&said).contains("was stopped from this session"), "{said}");
    assert!(place.local_runtime().is_none_or(|pid| !pid_alive(pid)), "not started again by the call");
    front.finish();
}

#[test]
fn an_add_machine_that_is_still_connecting_is_ended_by_another_tool_and_made_again_by_the_next_call() {
    let (place, _slurm) = slow_place("connecting-abandoned", "");
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    assert_eq!(front.ok("add_machine", json!({ "host": "lab", "julia": julia }))["state"], "connecting");
    assert_eq!(attempts(&place), 1);
    front.ok("stop_machine", json!({ "machine": "local" }));
    assert_eq!(front.ok("add_machine", json!({ "host": "lab", "julia": julia }))["state"], "connecting");
    assert_eq!(attempts(&place), 2, "the connection the first call left was ended, so this one made another");
    front.finish();
}

#[test]
fn an_update_of_a_saved_machine_that_is_still_connecting_is_not_used_by_anything_else() {
    let (place, _slurm) = slow_place("update-connecting", "");
    place.add_lab();
    let remembered = json!({ place.project.display().to_string(): { "machine": "lab", "folder": null } });
    std::fs::create_dir_all(place.dir.join("state-home/endeavor")).unwrap();
    std::fs::write(place.dir.join("state-home/endeavor/projects.json"), remembered.to_string()).unwrap();
    let mut front = place.front();
    front.initialize();
    let julia = place.julia.display().to_string();
    let first = front.ok("add_machine", json!({ "host": "lab2", "name": "lab", "julia": julia }));
    assert_eq!((first["state"].as_str(), first["saved"].clone()), (Some("connecting"), json!(true)), "{first}");
    assert_eq!(attempts(&place), 1);
    let listed = front.ok("list_machines", json!({}));
    assert_eq!(listed["machines"][0]["state"], "not connected", "the attempt with other settings is not the saved machine's connection: {listed}");
    // The session's own machine is reached with its saved settings, by a connection of its own.
    let status = front.ok("pluto_session_status", json!({}));
    assert_eq!(status["machine"], "lab", "{status}");
    assert_eq!(attempts(&place), 2, "a new connection, not the one being tried");
    assert_eq!(place.machines().find_by_name("lab").unwrap().unwrap().ssh_host, "lab");
    front.finish();
}

const NO_FOLDER: &str = "Give an absolute path: this server was not told the project folder.";

/// What the local engine was asked, by method and parameters.
fn local_engine_calls(place: &Place) -> Vec<(String, Value)> {
    let calls = place.local_bridge.seen().into_iter().filter(|s| s.line.starts_with("POST /adapter"));
    calls.map(|s| serde_json::from_slice::<Value>(&s.body).unwrap()).map(|c| (c["method"].as_str().unwrap().to_owned(), c["params"].clone())).collect()
}

fn project_files(place: &Place) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&place.project).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn a_front_without_a_folder_refuses_relative_paths_before_it_starts_a_runtime_or_makes_a_file() {
    let place = Place::new("no-folder");
    let mut front = place.front_without_folder();
    front.initialize();
    for (name, arguments) in [("new_notebook", json!({ "path": "hello.jl" })), ("new_notebook", json!({ "path": "./sub/../hello.jl" })), ("new_notebook", json!({})), ("new_notebook", json!({ "path": null })), ("open_notebook", json!({ "path": "hello.jl" }))] {
        let (failed, said) = front.call(name, arguments.clone());
        assert!(failed && said["error"] == "invalid_path" && said["message"] == NO_FOLDER, "{name} {arguments}: {said}");
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(place_none(&place), "a refused call started a runtime: {:?}", core_of(&place));
    assert!(project_files(&place).is_empty(), "{:?}", project_files(&place));

    // An absolute path starts the runtime and reaches the engine as given.
    let made = place.project.join("made.jl").display().to_string();
    front.call("new_notebook", json!({ "path": made }));
    assert_eq!(local_engine_calls(&place).into_iter().find(|(method, _)| method == "new").map(|(_, params)| params), Some(json!({ "path": made })));
    let existing = local_notebook(&place, "a.jl");
    let joined = front.ok("open_notebook", json!({ "path": existing }));
    assert_eq!(joined["already_open"], true, "{joined}");
    assert!(place.local_runtime().is_some_and(pid_alive));

    // The running runtime refuses a relative path too, for this session. It works in the home folder, not the project's,
    // and a session that says nothing about a folder is not refused: its relative paths start there.
    let (failed, said) = front.call("open_notebook", json!({ "path": "a.jl" }));
    assert!(failed && said["message"] == NO_FOLDER, "{said}");
    let home = place.env.iter().find(|(name, _)| name == "HOME").unwrap().1.clone();
    let record = read_record(&place.local_state);
    assert_eq!((record["folder"].as_str(), record["no_folder"].clone()), (Some(home.as_str()), json!(true)), "{record}");
    let port = record["port"].as_u64().unwrap() as u16;
    let other = other_agent(port, TOKEN, "someone-else", "open_notebook", json!({ "path": "a.jl" }));
    assert_eq!((other["error"].as_str(), other["message"].as_str()), (Some("file_not_found"), Some(format!("No file at '{home}/a.jl'").as_str())), "{other}");
    let relative: Vec<_> = local_engine_calls(&place).into_iter().filter(|(method, params)| matches!(method.as_str(), "open" | "new") && !params["path"].as_str().is_some_and(|path| path.starts_with('/'))).collect();
    assert!(relative.is_empty(), "the engine was asked to use a relative path: {relative:?}");

    // The tool list asks for absolute paths in the two descriptions that take one.
    let tools = front.request("tools/list", json!({}));
    let described = |name: &str| tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["inputSchema"]["properties"]["path"]["description"].as_str().unwrap().to_owned();
    assert!(described("open_notebook").starts_with("The notebook file, as an absolute path: this server was not told the project folder."), "{}", described("open_notebook"));
    assert!(described("new_notebook").starts_with("Where to create it: an absolute path ending in `.jl`."), "{}", described("new_notebook"));

    // The status names no project folder.
    let status = front.ok("pluto_session_status", json!({}));
    assert!(status.get("folder").is_none(), "{status}");
    let said = Command::new(env!("CARGO_BIN_EXE_endeavor")).args(["status", "--json", "--state-dir"]).arg(&place.local_state).env_clear().envs(place.env.iter().map(|(k, v)| (k, v))).output().unwrap();
    let report: Value = serde_json::from_slice(&said.stdout).unwrap();
    assert_eq!((report["runtime"]["state"].as_str(), report["runtime"]["folder"].as_str(), report["runtime"]["no_folder"].clone()), (Some("running"), Some(home.as_str()), json!(true)), "{report}");
    assert_eq!(project_files(&place), ["a.jl"], "only the file the test made");
    front.finish();
}

#[test]
fn a_front_without_a_folder_remembers_no_project_and_a_machine_resolves_relative_paths_as_before() {
    let place = Place::new("no-folder-machine");
    place.add_lab();
    let machine_notebook = place.notebook("machine.jl");
    let mut first = place.front_without_folder();
    first.initialize();
    let used = first.ok("use_machine", json!({ "machine": "lab", "folder": place.project.display().to_string() }));
    assert_eq!(used["state"], "ready", "{used}");
    let joined = first.ok("open_notebook", json!({ "path": "machine.jl" }));
    assert_eq!((joined["already_open"].clone(), joined["path"].as_str()), (json!(true), Some(machine_notebook.as_str())), "a relative path starts in the machine's folder: {joined}");
    assert_eq!(place.projects(), Value::Null, "nothing is remembered for a session without a folder");
    assert!(place_none(&place), "this computer's runtime was not started");

    // Back on this computer the session has no folder again.
    first.ok("use_machine", json!({ "machine": "local" }));
    let (failed, said) = first.call("open_notebook", json!({ "path": "machine.jl" }));
    assert!(failed && said["message"] == NO_FOLDER, "{said}");
    assert_eq!(place.projects(), Value::Null);
    first.ok("use_machine", json!({ "machine": "lab", "folder": place.project.display().to_string() }));
    assert_eq!(first.ok("open_notebook", json!({ "path": "machine.jl" }))["already_open"], true, "and the machine's folder is the one it was told");
    first.finish();

    // A second front without a folder does not come up on the machine.
    let mut second = place.front_without_folder();
    second.initialize();
    let status = second.ok("pluto_session_status", json!({}));
    assert!(status.get("machine").is_none(), "{status}");
    second.finish();
}

#[test]
fn a_front_with_a_folder_still_resolves_relative_paths_in_it() {
    let place = Place::new("with-folder");
    let path = local_notebook(&place, "a.jl");
    let mut front = place.front();
    front.initialize();
    let joined = front.ok("open_notebook", json!({ "path": "a.jl" }));
    assert_eq!((joined["already_open"].clone(), joined["path"].as_str()), (json!(true), Some(path.as_str())), "{joined}");
    assert!(read_record(&place.local_state)["folder"].as_str().is_some());
    front.finish();
}
