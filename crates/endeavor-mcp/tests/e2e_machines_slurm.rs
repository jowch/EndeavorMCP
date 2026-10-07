//! The machine tools of `endeavor mcp` over real ssh, real Slurm and real Julia:
//! an agent's harness (stdio) adds a cluster, is asked for resources before any
//! job is submitted, then gets one small job and works in a notebook inside it;
//! further sessions in the same project attach to the same job with no
//! `use_machine`; and a stop with `force` ends the job. It's ignored by default
//! and runs only when `ENDEAVOR_TEST_SSH_HOST` names a host this user can `ssh`
//! to with a key and that has Slurm (`localhost` on a single-node cluster is
//! one):
//!
//!     ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines_slurm -- --ignored --nocapture
//!
//! It submits one job of 1 CPU, 2 GB and 15 minutes to partition `LocalQ`
//! (`ENDEAVOR_TEST_SLURM_PARTITION` names another), and cancels it by its id if a
//! step fails. Julia is found as in `e2e_machines`, and the depot is
//! `e2e_client`'s, so run that test first or expect several minutes. The
//! fronts' state, config and cache folders, the machines file,
//! and the helper's install and state folders are under
//! `target/tmp/e2e-machines-slurm`, which the host has to see at the same path.
//! `HOME` stays the user's, where `ssh` finds its keys. The fronts' own local
//! runtime is pointed at a Julia that doesn't exist, so none is started here.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::front::Front;
use common::{find_julia, pid_alive, wait_for};
use serde_json::{Value, json};

fn user() -> String {
    std::env::var("USER").unwrap_or_else(|_| String::from_utf8_lossy(&Command::new("id").arg("-un").output().unwrap().stdout).trim().to_owned())
}

/// This user's jobs named `endeavor`, as `id|state|node|partition|time left`.
fn squeue(args: &[&str]) -> Vec<String> {
    let output = Command::new("squeue").args(["-h", "-u", &user(), "-n", "endeavor", "-o", "%i|%T|%N|%P|%L"]).args(args).output().expect("squeue runs");
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect()
}

fn listed(job: &str) -> Vec<String> {
    squeue(&["-j", job])
}

fn json_field(path: &Path, field: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v[field].as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A failed step leaves no Julia and no job behind: cancels the jobs this test recorded, and ends the runtime it started.
struct Ends {
    state: PathBuf,
    jobs: Arc<Mutex<Vec<String>>>,
}

impl Drop for Ends {
    fn drop(&mut self) {
        let mut jobs = self.jobs.lock().unwrap().clone();
        jobs.extend(["job.json", "runtime.json"].iter().filter_map(|f| json_field(&self.state.join(f), "job")));
        jobs.sort();
        jobs.dedup();
        for job in jobs {
            if !listed(&job).is_empty() {
                eprintln!("cleanup: cancelling job {job}");
                let _ = Command::new("scancel").arg(&job).output();
            }
        }
    }
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

fn machine<'a>(listed: &'a Value, name: &str) -> &'a Value {
    listed["machines"].as_array().and_then(|m| m.iter().find(|m| m["name"] == name)).unwrap_or_else(|| panic!("{name} is listed: {listed}"))
}

#[test]
#[ignore = "needs ENDEAVOR_TEST_SSH_HOST and Slurm, submits one small real job, several minutes the first time: ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_machines_slurm -- --ignored"]
fn the_machine_tools_over_real_slurm() {
    let Ok(host) = std::env::var("ENDEAVOR_TEST_SSH_HOST") else {
        eprintln!("SKIPPED: ENDEAVOR_TEST_SSH_HOST isn't set. Name a host this user can ssh to with a key and that has Slurm, such as localhost.");
        return;
    };
    if !Command::new("sinfo").arg("-h").output().is_ok_and(|o| o.status.success()) {
        eprintln!("SKIPPED: `sinfo` doesn't run here, so there's no Slurm to submit to.");
        return;
    }
    let Some((julia, app)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA, install Endeavor's own, or put julia on the PATH.");
        return;
    };
    let started = Instant::now();
    let partition = std::env::var("ENDEAVOR_TEST_SLURM_PARTITION").unwrap_or_else(|_| "LocalQ".into());
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-machines-slurm");
    let depot = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-client/depot");
    let jobs = Arc::new(Mutex::new(Vec::new()));
    // A job an earlier run left is this test's own to cancel: it is recorded in the test's own folder.
    drop(Ends { state: work.join("state"), jobs: jobs.clone() });
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
    let _ends = Ends { state: state.clone(), jobs: jobs.clone() };
    let before = squeue(&[]);
    eprintln!("host: {host}, julia: {}, partition: {partition}; this user's endeavor jobs before: {before:?}", julia.display());

    let front = || {
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
        front
    };
    let mut one = front();

    // Adding the cluster: Slurm is found, the partitions come with their limits, and the host's name for it is saved.
    let added = one.ok("add_machine", json!({ "host": host, "name": "e2e-slurm", "julia": julia.display().to_string(), "slurm": true, "install": true }));
    eprintln!("[{:?}] add_machine: {added}", started.elapsed());
    assert_eq!((added["state"].as_str(), added["saved"].clone(), added["slurm"].clone(), added["cluster"].clone(), added["runs_in"].clone()), (Some("connected"), json!(true), json!(true), json!(true), json!("slurm_jobs")), "{added}");
    let partitions = added["partitions"].as_array().unwrap_or_else(|| panic!("{added}"));
    let ours = partitions.iter().find(|p| p["name"] == json!(partition)).unwrap_or_else(|| panic!("partition {partition} is listed: {added}"));
    assert!(ours["cpus"].as_u64().is_some_and(|c| c >= 1) && ours["memory_gb"].as_u64().is_some_and(|m| m >= 2), "{ours}");
    assert!(added["message"].as_str().unwrap().contains("Slurm"), "{added}");
    let listing = one.ok("list_machines", json!({}));
    let entry = machine(&listing, "e2e-slurm");
    assert_eq!((entry["cluster"].clone(), entry["state"].as_str(), entry["this_session"].clone()), (json!(true), Some("not connected"), json!(false)), "{listing}");

    // No resources: asked for, nothing submitted, the session where it was.
    let asked = one.ok("use_machine", json!({ "machine": "e2e-slurm", "folder": notebooks.display().to_string() }));
    eprintln!("[{:?}] use_machine without resources: {asked}", started.elapsed());
    assert_eq!((asked["state"].as_str(), asked["needs_job"].clone(), asked["ready"].clone()), (Some("needs_job"), json!(true), json!(false)), "{asked}");
    assert!(asked["message"].as_str().unwrap().contains("nothing was submitted"), "{asked}");
    assert_eq!(squeue(&[]), before, "no job was submitted");
    assert!(!state.join("job.json").exists());
    let listing = one.ok("list_machines", json!({}));
    assert_eq!((machine(&listing, "e2e-slurm")["this_session"].clone(), listing["this_session"]["machine"].clone()), (json!(false), json!("local")), "the session has not moved: {listing}");

    // The job: 1 CPU, 2 GB, 15 minutes.
    let asked_at = Instant::now();
    let used = one.ok("use_machine", json!({ "machine": "e2e-slurm", "folder": notebooks.display().to_string(), "partition": partition, "cpus": 1, "memory_gb": 2, "hours": 0.25 }));
    eprintln!("[{:?}] use_machine: {used}", started.elapsed());
    assert!(asked_at.elapsed() < Duration::from_secs(50), "use_machine answers within its limit: {:?}", asked_at.elapsed());
    let job = used["job"]["id"].as_str().unwrap_or_else(|| panic!("the result names the job: {used}")).to_owned();
    jobs.lock().unwrap().push(job.clone());
    let mine: Vec<String> = squeue(&[]).into_iter().filter(|l| !before.contains(l)).collect();
    assert_eq!(mine.iter().map(|l| l.split('|').next().unwrap()).collect::<Vec<_>>(), vec![job.as_str()], "one new job: {mine:?}");
    assert!(matches!(used["state"].as_str(), Some("queued" | "starting" | "ready")), "{used}");
    let mut seen = vec![format!("use_machine: {}", used["state"])];
    let status = loop {
        let status = one.ok("pluto_session_status", json!({}));
        let note = format!("{} / {} / {}", status["state"], status["queue"], status["message"]);
        if seen.last() != Some(&note) {
            eprintln!("[{:?}] pluto_session_status: {status}", started.elapsed());
            seen.push(note);
        }
        if status.get("browser_url").is_some() {
            break status;
        }
        assert!(matches!(status["state"].as_str(), Some("queued" | "starting" | "connected")), "{status}");
        assert!(status["machine"] == "e2e-slurm" && status["job"]["id"] == json!(job), "{status}");
        assert!(started.elapsed() < Duration::from_secs(1500), "{status}");
        std::thread::sleep(Duration::from_millis(500));
    };
    let ready = if used["ready"] == true { used.clone() } else { one.ok("use_machine", json!({ "machine": "e2e-slurm" })) };
    eprintln!("[{:?}] ready: {ready}\nstatus: {status}", started.elapsed());
    assert_eq!((ready["state"].as_str(), ready["ready"].clone()), (Some("ready"), json!(true)), "{ready}");
    let (node, ends_in) = (ready["job"]["node"].as_str().unwrap_or_default(), ready["job"]["ends_in_minutes"].as_u64());
    assert!(!node.is_empty() && ready["node"] == ready["job"]["node"], "{ready}");
    assert!(ends_in.is_some_and(|m| (12..=15).contains(&m)), "the job ends in about 15 minutes: {ready}");
    assert_eq!(status["job"]["id"], json!(job), "{status}");
    assert_eq!((status["job"]["node"].as_str(), status["machine"].as_str()), (Some(node), Some("e2e-slurm")), "{status}");
    assert!(status["job"]["ends_in_minutes"].as_u64().is_some_and(|m| (12..=15).contains(&m)), "{status}");
    let line = listed(&job);
    assert_eq!(line.len(), 1, "{line:?}");
    let fields: Vec<&str> = line[0].split('|').collect();
    assert_eq!((fields[1], fields[3]), ("RUNNING", partition.as_str()), "{line:?}");
    let machines = one.ok("list_machines", json!({}));
    let entry = machine(&machines, "e2e-slurm");
    assert_eq!((entry["state"].as_str(), entry["this_session"].clone()), (Some("ready"), json!(true)), "{machines}");
    eprintln!("states seen while waiting: {seen:?}");

    let remote_port = std::fs::read_to_string(state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["port"].as_u64()).expect("the runtime's port in its record");
    assert_eq!(ready["remote_port"], remote_port, "{ready}");
    assert_eq!(status["remote_port"], remote_port, "{status}");
    let message = ready["message"].as_str().unwrap();
    assert!(message.contains(&format!("node {node}, port {remote_port}")) && !message.contains("ssh -L"), "a compute node's port is given, and no command is promised: {message}");
    let page = ready["browser_url"].as_str().unwrap().to_owned();
    let (port, token) = page.strip_prefix("http://localhost:").unwrap().split_once("/?token=").map(|(p, t)| (p.parse::<u16>().unwrap(), t.to_owned())).unwrap();

    // The notebook runs inside the job.
    let created = one.ok("new_notebook", json!({ "path": "slurm.jl" }));
    let notebook = created["notebook_id"].as_str().expect("a notebook").to_owned();
    let notebook_path = notebooks.join("slurm.jl").display().to_string();
    assert_eq!(created["path"], json!(notebook_path), "{created}");
    let order = one.ok("get_cell_order", json!({ "notebook_id": notebook }));
    let last = order["cell_ids"].as_array().unwrap().last().unwrap().clone();
    let added = one.ok("add_cell", json!({ "notebook_id": notebook, "code": "ENV[\"SLURM_JOB_ID\"]", "after_cell_id": last }));
    let cell = added["cell_id"].as_str().unwrap().to_owned();
    one.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": cell, "wait_for_completion": true }));
    let read = one.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
    assert_eq!((read["output"].as_str().map(|o| o.trim_matches('"')), &read["errored"]), (Some(job.as_str()), &json!(false)), "the cell ran in job {job}: {read}");
    let shell = one.ok("run_shell", json!({ "command": "hostname; echo job=$SLURM_JOB_ID" }));
    eprintln!("run_shell: {shell}");
    assert!(shell.to_string().contains(node) && shell.to_string().contains(&format!("job={job}")), "the shell is on the node, in the job: {shell}");
    let (line, head, _) = get(port, &format!("/edit?id={notebook}&token={token}"), "");
    assert_eq!(line, "HTTP/1.1 303 See Other", "{head}");
    let cookie = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}")).split(';').next().unwrap().to_owned();
    let (line, head, body) = get(port, &format!("/edit?id={notebook}"), &format!("Cookie: {cookie}\r\nSec-Fetch-Site: none\r\n"));
    assert_eq!(line, "HTTP/1.1 200 OK", "{head}");
    assert!(body.contains("Pluto"));
    eprintln!("[{:?}] the notebook ran in job {job} on {node}, and the page answers", started.elapsed());

    // Each front connects for itself, so its first status may still say it is connecting: ask until the runtime is there.
    let up = |front: &mut Front| {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let status = front.ok("pluto_session_status", json!({}));
            if status.get("browser_url").is_some() {
                return status;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(500));
        }
    };

    // A second session in the project attaches to the same job with no `use_machine`, and sees the first one's notebook.
    let mut two = front();
    let attached = up(&mut two);
    eprintln!("[{:?}] second front's pluto_session_status: {attached}", started.elapsed());
    assert_eq!((attached["machine"].as_str(), attached["job"]["id"].as_str(), attached["job"]["node"].as_str()), (Some("e2e-slurm"), Some(job.as_str()), Some(node)), "{attached}");
    assert!(attached["job"]["ends_in_minutes"].as_u64().is_some_and(|m| (10..=15).contains(&m)), "the job's end is known to a front that attached to a running job: {attached}");
    let seen_by_two = two.ok("list_notebooks", json!({}));
    assert!(seen_by_two.as_array().is_some_and(|l| l.iter().any(|nb| nb["notebook_id"] == json!(notebook))), "{seen_by_two}");
    assert_eq!(squeue(&[]).iter().filter(|l| !before.contains(l)).count(), 1, "still one job");
    let again = two.ok("use_machine", json!({ "machine": "e2e-slurm" }));
    eprintln!("second front's use_machine: {again}");
    assert_eq!((again["state"].as_str(), again["already_running"].clone(), again["job"]["id"].as_str(), again["job"]["node"].as_str()), (Some("ready"), json!(true), Some(job.as_str()), Some(node)), "{again}");
    assert!(again["job"]["ends_in_minutes"].as_u64().is_some(), "{again}");
    assert_eq!(squeue(&[]).iter().filter(|l| !before.contains(l)).count(), 1, "still one job");
    two.finish();
    assert_eq!(listed(&job).len(), 1, "the job runs on");

    // A third session while the first is still there, then the first goes: nothing detached the job.
    let mut three = front();
    let attached = up(&mut three);
    assert_eq!((attached["machine"].as_str(), attached["job"]["id"].as_str()), (Some("e2e-slurm"), Some(job.as_str())), "{attached}");
    one.finish();
    assert_eq!(listed(&job).len(), 1, "the job runs on after the first front is gone");
    let still = three.ok("list_notebooks", json!({}));
    assert!(still.as_array().is_some_and(|l| l.iter().any(|nb| nb["notebook_id"] == json!(notebook))), "{still}");
    assert_eq!(squeue(&[]).iter().filter(|l| !before.contains(l)).count(), 1, "still one job");
    let reading = three.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
    assert_eq!(reading["output"].as_str().map(|o| o.trim_matches('"')), Some(job.as_str()), "{reading}");

    // A stop is refused while another session was active, and with `force` ends the job.
    let mut four = front();
    four.ok("open_notebook", json!({ "path": notebook_path }));
    let refused = three.ok("stop_machine", json!({ "machine": "e2e-slurm" }));
    eprintln!("[{:?}] stop_machine without force: {refused}", started.elapsed());
    assert_eq!(refused["stopped"], json!(false), "{refused}");
    assert!(refused["active_sessions"].as_u64().is_some_and(|n| n >= 1), "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("active on") && refused["message"].as_str().unwrap().contains("force true"), "{refused}");
    assert_eq!(listed(&job).len(), 1, "nothing was cancelled");
    four.finish();

    let runtime_pid = std::fs::read_to_string(state.join("runtime.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).map(|p| p as i32);
    let stopped = three.ok("stop_machine", json!({ "machine": "e2e-slurm", "force": true }));
    eprintln!("[{:?}] stop_machine with force: {stopped}", started.elapsed());
    assert_eq!(stopped["stopped"], json!(true), "{stopped}");
    assert!(stopped["message"].as_str().unwrap().contains("Slurm job was cancelled"), "{stopped}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !listed(&job).is_empty() {
        assert!(Instant::now() < deadline, "job {job} is still {:?} after the stop", listed(&job));
        std::thread::sleep(Duration::from_millis(250));
    }
    if let Some(pid) = runtime_pid {
        wait_for("the runtime to end", || !pid_alive(pid));
    }
    let (failed, said) = three.call("list_notebooks", json!({}));
    eprintln!("a notebook call after the stop: {said}");
    assert!(failed && said.as_str().is_some_and(|t| t.contains("`use_machine`") && t.contains("stopped")), "{said}");
    let listing = three.ok("list_machines", json!({}));
    let entry = machine(&listing, "e2e-slurm");
    assert_eq!(entry["state"].as_str(), Some("connected"), "{listing}");
    three.finish();

    // Nothing is left: the front's end took its connection with it.
    wait_for("the helper to go", || Command::new("pgrep").arg("-f").arg("--").arg(format!("connect --state-dir {}", state.display())).output().unwrap().stdout.is_empty());
    assert!(!state.join("runtime.json").exists() && !state.join("job.json").exists());
    assert_eq!(squeue(&[]), before, "no job of this test is left");
    eprintln!("[{:?}] stopped; nothing left", started.elapsed());
}
