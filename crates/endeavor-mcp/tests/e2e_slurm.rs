//! The cluster path over real ssh, real Slurm and real Julia: the client
//! library connects to a login node, the helper submits a small job, the runtime
//! starts in it, the agent's MCP calls reach it through the local listener, a
//! second client joins the same job, and a stop ends the job. It's ignored by
//! default and runs only when `ENDEAVOR_TEST_SSH_HOST` names a host this user
//! can `ssh` to with a key and that has Slurm (`localhost` on a single-node
//! cluster is one):
//!
//!     ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_slurm -- --ignored --nocapture
//!
//! It submits one job of 1 CPU, 2 GB and 15 minutes to partition `LocalQ`
//! (`ENDEAVOR_TEST_SLURM_PARTITION` names another). Julia is found as in
//! `e2e_client`, and the depot is that test's, so the first run after a clean
//! target folder takes several minutes. Its install and state folders are under
//! `target/tmp/e2e-slurm`, which the host has to see at the same path.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::find_julia;
use endeavor_mcp::client::{Auth, Cancel, Channel, Cluster, Event, Listener, Notice, Options, Runtime, Server, StartOptions, Transport, connect, start};
use serde_json::{Value, json};
use wire::files::{Reply, Request, RuntimeState};
use wire::slurm::{JobRequest, Resources};

/// What a client's connect and start told, in order, with the time since the test began.
#[derive(Clone)]
struct Log {
    events: Arc<Mutex<Vec<Event>>>,
    began: Instant,
    who: &'static str,
}

impl Log {
    fn new(who: &'static str, began: Instant) -> Log {
        Log { events: Arc::default(), began, who }
    }

    fn on(&self) -> impl Fn(Event) + Send + Sync + 'static {
        let log = self.clone();
        move |event| {
            eprintln!("[{:?}] {}: {event:?}", log.began.elapsed(), log.who);
            log.events.lock().unwrap().push(event);
        }
    }

    fn all(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn jobs(&self) -> Vec<String> {
        self.all().into_iter().filter_map(|e| if let Event::Submitted { job, .. } = e { Some(job) } else { None }).collect()
    }

    fn progress(&self) -> Vec<String> {
        self.all().into_iter().filter_map(|e| if let Event::Progress(line) = e { Some(line) } else { None }).collect()
    }
}

fn user() -> String {
    std::env::var("USER").unwrap_or_else(|_| String::from_utf8_lossy(&Command::new("id").arg("-un").output().unwrap().stdout).trim().to_owned())
}

/// `squeue`'s lines for `args`, as `id|name|user|state|node|partition`.
fn squeue(args: &[&str]) -> Vec<String> {
    let output = Command::new("squeue").args(["-h", "-o", "%i|%j|%u|%T|%N|%P"]).args(args).output().expect("squeue runs");
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect()
}

/// The ids of this user's jobs named `endeavor`.
fn endeavor_jobs() -> Vec<String> {
    let mut ids: Vec<String> = squeue(&["-u", &user(), "-n", "endeavor"]).iter().map(|l| l.split('|').next().unwrap().to_owned()).collect();
    ids.sort();
    ids
}

/// The processes (pid, command line) whose command line has `needle`, apart from this one.
fn processes(needle: &str) -> Vec<(i32, String)> {
    let output = Command::new("ps").args(["-eo", "pid=,args="]).output().expect("ps runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim_start().split_once(' ').map(|(pid, args)| (pid.parse::<i32>().unwrap(), args.trim().to_owned())))
        .filter(|(pid, args)| args.contains(needle) && *pid != std::process::id() as i32 && !args.starts_with("ps "))
        .collect()
}

/// `root` and everything below it, by parent pid.
fn descendants(root: i32) -> Vec<i32> {
    let output = Command::new("ps").args(["-eo", "pid=,ppid="]).output().expect("ps runs");
    let pairs: Vec<(i32, i32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace().map(|w| w.parse::<i32>().unwrap());
            Some((words.next()?, words.next()?))
        })
        .collect();
    let mut found = vec![root];
    let mut next = 0;
    while next < found.len() {
        let parent = found[next];
        found.extend(pairs.iter().filter(|(_, ppid)| *ppid == parent).map(|(pid, _)| *pid));
        next += 1;
    }
    found
}

fn alive(pid: i32) -> bool {
    // A zombie is gone for this purpose.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.rsplit(") ").next().unwrap_or_default().starts_with('Z'))
}

fn recorded_job(state: &Path, file: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(state.join(file)).ok()?).ok()?;
    v["job"].as_str().filter(|j| !j.is_empty()).map(str::to_owned)
}

fn cancel(job: &str) {
    let _ = Command::new("scancel").arg(job).output();
}

/// A failed step leaves no job behind: cancels the ones this test submitted that are still listed.
struct Cancels {
    state: PathBuf,
    logs: Vec<Log>,
}

impl Drop for Cancels {
    fn drop(&mut self) {
        let mut jobs: Vec<String> = self.logs.iter().flat_map(Log::jobs).collect();
        jobs.extend(["job.json", "runtime.json"].iter().filter_map(|f| recorded_job(&self.state, f)));
        jobs.sort();
        jobs.dedup();
        for job in jobs {
            if squeue(&["-j", &job, "-u", &user()]).iter().any(|l| l.split('|').nth(1) == Some("endeavor")) {
                eprintln!("cleanup: cancelling job {job}");
                cancel(&job);
            }
        }
    }
}

/// An agent on a listener's port, with the session id `initialize` gives.
struct Agent {
    port: u16,
    token: String,
    id: u64,
    session: Option<String>,
}

impl Agent {
    fn new(port: u16, token: &str) -> Agent {
        Agent { port, token: token.to_owned(), id: 0, session: None }
    }

    /// POST `message` to `/mcp` as the agent does: the status line, the head and the body.
    fn post(&self, token: &str, message: &Value) -> (String, String, Value) {
        let body = message.to_string();
        let session = self.session.as_deref().map_or(String::new(), |id| format!("Mcp-Session-Id: {id}\r\n"));
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
        write!(
            socket,
            "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\n\r\n{body}",
            self.port,
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        (head.lines().next().unwrap_or_default().to_owned(), head.to_owned(), serde_json::from_str(body).unwrap_or(Value::Null))
    }

    fn mcp(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let (status, head, reply) = self.post(&self.token.clone(), &json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params }));
        assert_eq!(status, "HTTP/1.1 200 OK", "{method}: {reply}");
        if method == "initialize" {
            self.session = head.lines().find_map(|line| line.strip_prefix("Mcp-Session-Id: ")).map(str::to_owned);
        }
        reply
    }

    fn initialize(&mut self) -> Value {
        self.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e-slurm", "version": "0" } }))
    }

    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.mcp("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {reply}"));
        assert!(reply["result"]["isError"] != true, "{name}({arguments}): {text}");
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
    }

    fn notebooks(&mut self) -> Vec<Value> {
        let listed = self.ok("list_notebooks", json!({}));
        listed.as_array().unwrap_or_else(|| panic!("list_notebooks: {listed}")).clone()
    }
}

/// One client: its channel to a helper on the host, its listener, and what it was told.
struct Client {
    channel: Arc<Channel>,
    listener: Arc<Listener>,
    runtime: Runtime,
    lost: mpsc::Receiver<Notice>,
}

fn step<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    eprintln!("-- {name}");
    let result = f();
    eprintln!("   {:.1}s", started.elapsed().as_secs_f64());
    result
}

#[test]
#[ignore = "needs ENDEAVOR_TEST_SSH_HOST and Slurm, submits one small real job, several minutes the first time: ENDEAVOR_TEST_SSH_HOST=localhost cargo test -p endeavor-mcp --test e2e_slurm -- --ignored"]
fn a_runtime_in_a_slurm_job() {
    let Ok(host) = std::env::var("ENDEAVOR_TEST_SSH_HOST") else {
        eprintln!("SKIPPED: ENDEAVOR_TEST_SSH_HOST isn't set. Name a host this user can ssh to with a key and that has Slurm, such as localhost.");
        return;
    };
    if !Command::new("sinfo").arg("-h").output().is_ok_and(|o| o.status.success()) {
        eprintln!("SKIPPED: `sinfo` doesn't run here, so there's no Slurm to submit to.");
        return;
    }
    let Some((julia, _)) = find_julia() else {
        eprintln!("SKIPPED: no Julia. Set ENDEAVOR_E2E_JULIA or put julia on the PATH.");
        return;
    };
    let began = Instant::now();
    let partition = std::env::var("ENDEAVOR_TEST_SLURM_PARTITION").unwrap_or_else(|_| "LocalQ".into());
    eprintln!("host: {host}, julia: {}, partition: {partition}, this machine: {}", julia.display(), String::from_utf8_lossy(&Command::new("hostname").output().unwrap().stdout).trim());
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let work = tmp.join("e2e-slurm");
    let (root, state, project) = (work.join("root"), work.join("state"), work.join("project"));
    // The depot e2e_client built, which already has Pluto's packages.
    let depot = tmp.join("e2e-client/depot");

    // A job an earlier run left is ours to cancel: it's recorded in this test's own folder.
    for file in ["job.json", "runtime.json"] {
        if let Some(job) = recorded_job(&state, file)
            && squeue(&["-j", &job, "-u", &user()]).iter().any(|l| l.split('|').nth(1) == Some("endeavor"))
        {
            eprintln!("an earlier run's job {job} is still listed; cancelling it");
            cancel(&job);
        }
    }
    for pid in processes(work.to_str().unwrap()).into_iter().map(|(pid, _)| pid) {
        eprintln!("an earlier run's process {pid} is still there");
    }
    for folder in [&root, &state, &project] {
        let _ = std::fs::remove_dir_all(folder);
    }
    for folder in [&depot, &project] {
        std::fs::create_dir_all(folder).unwrap();
    }
    let (root, project, depot) = (root.canonicalize().unwrap_or(root), project.canonicalize().unwrap(), depot.canonicalize().unwrap());
    std::fs::create_dir_all(&state).unwrap();
    let state = state.canonicalize().unwrap();
    let before = endeavor_jobs();
    eprintln!("this user's endeavor jobs before: {before:?}");

    let (first, second, third) = (Log::new("first", began), Log::new("second", began), Log::new("third", began));
    let _cancels = Cancels { state: state.clone(), logs: vec![first.clone(), second.clone(), third.clone()] };

    let resources = Resources { partition: Some(partition.clone()), cpus: 1, mem_gb: 2, minutes: 15, gres: None, extra: Vec::new() };
    let cluster = Cluster { resources: resources.clone(), depot: Some(depot.display().to_string()), ..Default::default() };
    let request: JobRequest = cluster.job(&resources);
    let server = Server { id: "e2e-slurm".into(), ssh_host: host.clone(), julia: Some(julia.display().to_string()), cluster: Some(cluster), ..Default::default() };
    assert_eq!(server.launcher(), endeavor_mcp::client::Launcher::Slurm);
    let helper = |_: &str, _: &str| Ok(PathBuf::from(env!("CARGO_BIN_EXE_endeavor")));
    let options = Options { auth: Auth::Batch, root: root.display().to_string(), state: state.display().to_string(), depot: format!("{}:", depot.display()), exit_idle: false, julia_when_needed: false, allow_install: true, helper: &helper, launcher: None };

    let join = |log: &Log| -> Client {
        let (channel, hello) = connect(&server, &Transport::for_server(&server), &options, &Cancel::default(), &log.on()).map_err(|e| e.message).expect("connect over ssh");
        eprintln!("[{:?}] {}: hello from {}, home {}, slurm {}", began.elapsed(), log.who, hello.node, hello.home, hello.slurm);
        assert!(hello.slurm, "the login node has Slurm's commands");
        let listener = Listener::start(&host).unwrap();
        let channel = Arc::new(channel);
        let (lost_tx, lost) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        std::thread::spawn({
            let (channel, listener, log, request) = (channel.clone(), listener.clone(), log.clone(), request.clone());
            move || drop(ready_tx.send(start(&channel, &listener, &StartOptions { job: Some(request), install: true, ..StartOptions::default() }, &log.on(), move |notice| drop(lost_tx.send(notice)))))
        });
        let runtime = ready.recv_timeout(Duration::from_secs(1500)).unwrap_or_else(|_| panic!("{} is ready in time", log.who)).expect("start");
        Client { channel, listener, runtime, lost }
    };

    // What the helper finds without taking the runtime over: the app asks this before it connects.
    let found = |client: &Client| match client.channel.files(Request::Runtime) {
        Ok(Reply::Runtime { runtime }) => runtime,
        other => panic!("the helper answers Runtime: {other:?}"),
    };

    let one = step("connect, start in a Slurm job, ready", || {
        let one = join(&first);
        let jobs = first.jobs();
        assert_eq!(jobs.len(), 1, "one job submitted: {:?}", first.all());
        let runtime = &one.runtime;
        eprintln!("job {}, ready {:?} after the test began; node {} (job's node {:?}), pid {}", jobs[0], began.elapsed(), runtime.node, runtime.job.as_ref().map(|j| &j.node), runtime.pid);
        assert!(!runtime.reattached, "no runtime was running in {}", state.display());
        assert_eq!(runtime.job.as_ref().map(|j| j.id.as_str()), Some(jobs[0].as_str()), "the runtime says which job it's in");
        assert_eq!(runtime.mcp_url, format!("http://127.0.0.1:{}/mcp", runtime.port));
        assert!(runtime.page_url.contains(&runtime.token) && runtime.token.len() >= 32);
        let events = first.all();
        let at = |f: &dyn Fn(&Event) -> bool| events.iter().position(f).unwrap_or_else(|| panic!("no such event in {events:?}"));
        assert!(at(&|e| matches!(e, Event::Submitted { .. })) < at(&|e| matches!(e, Event::Started { .. })));
        let lines = first.progress();
        let reached = lines.iter().find(|l| l.starts_with("Reached ")).unwrap_or_else(|| panic!("the helper says how it reached the node: {lines:?}"));
        eprintln!("relay: {reached}; route in the job info: {:?}", runtime.job.as_ref().map(|j| &j.route));
        one
    });
    let job = first.jobs()[0].clone();

    step("the job is running and ours; what the node names say", || {
        let listed = squeue(&["-j", &job]);
        assert_eq!(listed.len(), 1, "{listed:?}");
        let fields: Vec<&str> = listed[0].split('|').collect();
        assert_eq!((fields[0], fields[1], fields[2], fields[3], fields[5]), (job.as_str(), "endeavor", user().as_str(), "RUNNING", partition.as_str()), "{listed:?}");
        let recorded: Value = serde_json::from_str(&std::fs::read_to_string(state.join("runtime.json")).unwrap()).unwrap();
        eprintln!("runtime.json: {recorded}");
        eprintln!("node names: squeue %N {:?}; runtime.json node {:?}; ready node {:?}; job info node {:?}", fields[4], recorded["node"], one.runtime.node, one.runtime.job.as_ref().map(|j| &j.node));
        assert_eq!(recorded["job"], json!(job));
        let found = found(&one);
        eprintln!("the helper's own check says: {found:?}");
        assert!(matches!(&found, RuntimeState::Running { job: Some(j), .. } if j.id == job), "{found:?}");
        assert_eq!(recorded["launcher"], "slurm");
        assert!(state.join("job.sh").is_file());
        assert!(std::fs::metadata(state.join("job.json")).is_err(), "job.json is gone once the runtime is up");
    });

    let mut agent = Agent::new(one.runtime.port, &one.runtime.token);
    let (notebook, cell) = step("MCP through the listener: handshake, token, a notebook, a cell run", || {
        let init = agent.initialize();
        assert_eq!(init["result"]["serverInfo"]["name"], "endeavor-runtime", "{init}");
        let listed = agent.mcp("tools/list", json!({}));
        let names: Vec<&str> = listed["result"]["tools"].as_array().unwrap_or_else(|| panic!("{listed}")).iter().filter_map(|t| t["name"].as_str()).collect();
        for tool in ["list_notebooks", "new_notebook", "add_cell", "execute_cell"] {
            assert!(names.contains(&tool), "tools/list has {tool}: {names:?}");
        }
        let (status, _, _) = agent.post("wrong", &json!({ "jsonrpc": "2.0", "id": 99, "method": "tools/list", "params": {} }));
        assert_eq!(status, "HTTP/1.1 401 Unauthorized", "the token is checked");

        let created = agent.ok("new_notebook", json!({ "path": project.join("slurm.jl").display().to_string() }));
        let notebook = created["notebook_id"].as_str().unwrap_or_else(|| panic!("new_notebook: {created}")).to_owned();
        let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }));
        let last = order["cell_ids"].as_array().and_then(|ids| ids.last()).unwrap_or_else(|| panic!("get_cell_order: {order}")).clone();
        let added = agent.ok("add_cell", json!({ "notebook_id": notebook, "code": "x = 21 * 2", "after_cell_id": last }));
        let cell = added["cell_id"].as_str().unwrap_or_else(|| panic!("add_cell: {added}")).to_owned();
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": cell, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
        assert_eq!((&read["output"], &read["errored"]), (&json!("42"), &json!(false)), "{read}");
        (notebook, cell)
    });

    let before_second = processes(work.to_str().unwrap());
    let two = step("a second client joins the same job", || {
        let two = join(&second);
        assert!(matches!(found(&two), RuntimeState::Running { .. }));
        assert!(two.runtime.reattached, "the second client attaches to the runtime");
        assert_eq!(two.runtime.job.as_ref().map(|j| j.id.as_str()), Some(job.as_str()));
        assert_eq!(two.runtime.pid, one.runtime.pid);
        assert_eq!(two.runtime.token, one.runtime.token);
        assert_ne!(two.runtime.port, one.runtime.port, "each client has its own listener");
        assert!(second.jobs().is_empty(), "no second job: {:?}", second.all());
        let mut ours = endeavor_jobs();
        ours.retain(|id| !before.contains(id));
        assert_eq!(ours, vec![job.clone()], "one job of this test; before: {before:?}");
        let mut other = Agent::new(two.runtime.port, &two.runtime.token);
        other.initialize();
        let seen = other.notebooks();
        assert!(seen.iter().any(|nb| nb["notebook_id"] == json!(notebook)), "the second client sees the first's notebook: {seen:?}");
        eprintln!("second route: {:?}; {:?}", two.runtime.job.as_ref().map(|j| &j.route), second.progress().iter().find(|l| l.starts_with("Reached ")));
        two
    });
    let relays = |name: &str| processes(name).into_iter().filter(|(_, args)| args.contains("relay --state-dir")).count();
    eprintln!("relay processes with both clients: {}", relays(state.to_str().unwrap()));

    step("the second client detaches; the first goes on", || {
        two.channel.detach();
        assert!(two.channel.closed().is_none(), "the second helper ended because the client let it go");
        assert!(two.lost.try_recv().is_err());
        drop(two.listener);
        assert_eq!(squeue(&["-j", &job, "-u", &user()]).len(), 1, "the job runs on");
        assert!(alive(one.runtime.pid as i32), "the runtime runs on");
        let seen = agent.notebooks();
        assert!(seen.iter().any(|nb| nb["notebook_id"] == json!(notebook)), "{seen:?}");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": cell }));
        assert_eq!(read["output"], json!("42"), "{read}");
        std::thread::sleep(Duration::from_secs(1));
        let left = processes(state.to_str().unwrap()).len();
        eprintln!("relay processes after the second detached: {}; processes naming the state folder: {left} (before the second joined: {})", relays(state.to_str().unwrap()), before_second.len());
        assert_eq!(processes(state.to_str().unwrap()).len(), before_second.iter().filter(|(_, a)| a.contains(state.to_str().unwrap())).count(), "the second client's relay and helper are gone");
    });

    step("the first client stops the runtime; the job ends", || {
        // What belongs to the job, to be gone after the stop: the runtime's tree, and anything naming this test's folders.
        let mut mine: Vec<i32> = descendants(one.runtime.pid as i32);
        mine.extend(processes(work.to_str().unwrap()).into_iter().map(|(pid, _)| pid));
        mine.sort();
        mine.dedup();
        let named: Vec<String> = mine.iter().map(|pid| format!("{pid}: {}", std::fs::read(format!("/proc/{pid}/cmdline")).map(|c| String::from_utf8_lossy(&c).replace('\0', " ")).unwrap_or_default().chars().take(150).collect::<String>())).collect();
        eprintln!("processes of the job before the stop:\n  {}", named.join("\n  "));

        let stopping = Instant::now();
        one.channel.stop().expect("stop");
        eprintln!("stop took {:?}", stopping.elapsed());
        assert!(stopping.elapsed() < Duration::from_secs(25), "stop returned on the helper's Stopped, not its timeout");
        assert!(one.lost.try_recv().is_err(), "stopping on purpose is no notice");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !squeue(&["-j", &job, "-u", &user()]).iter().all(|l| matches!(l.split('|').nth(3), Some("COMPLETING" | "CANCELLED"))) {
            assert!(Instant::now() < deadline, "job {job} is still {:?} after stop", squeue(&["-j", &job]));
            std::thread::sleep(Duration::from_millis(250));
        }
        eprintln!("job {job} after the stop: {:?}", squeue(&["-j", &job]));
        assert_eq!(found(&one), RuntimeState::NotRunning);
        one.channel.detach();
        assert!(one.channel.closed().is_none(), "the helper ended because the client let it go");
        for file in ["runtime.json", "job.json"] {
            assert!(!state.join(file).exists(), "{file} is removed by the stop");
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let left = loop {
            let left: Vec<i32> = mine.iter().copied().filter(|pid| alive(*pid)).collect();
            if left.is_empty() || Instant::now() > deadline {
                break left;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        assert!(left.is_empty(), "still running after the job ended: {left:?}");
        let rest = processes(work.to_str().unwrap());
        assert!(rest.is_empty(), "processes naming this test's folders are left: {rest:?}");
        assert_eq!(squeue(&["-j", &job]).iter().filter(|l| l.split('|').nth(3) == Some("RUNNING")).count(), 0);
        let mut ours = endeavor_jobs();
        ours.retain(|id| !before.contains(id));
        assert!(ours.is_empty() || ours == vec![job.clone()], "{ours:?}");
        eprintln!("log tail:\n{}", std::fs::read_to_string(state.join("runtime.log")).unwrap_or_default().lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        drop(one.listener);
    });

    step("the job is cancelled from outside while a client is attached", || {
        let three = join(&third);
        let jobs = third.jobs();
        assert_eq!(jobs.len(), 1, "a new job, the old one is over: {:?}", third.all());
        assert_ne!(jobs[0], job);
        assert!(!three.runtime.reattached);
        let mut mine = descendants(three.runtime.pid as i32);
        mine.extend(processes(work.to_str().unwrap()).into_iter().map(|(pid, _)| pid));
        mine.sort();
        mine.dedup();
        let cancelled = Instant::now();
        cancel(&jobs[0]);
        let notice = three.lost.recv_timeout(Duration::from_secs(60)).expect("the client is told the job ended");
        eprintln!("told after {:?}: {notice:?}", cancelled.elapsed());
        let Notice::Died(why) = notice else { panic!("expected Died: {notice:?}") };
        assert!(why.to_lowercase().contains("cancel"), "the reason says the job was cancelled: {why:?}");
        let deadline = Instant::now() + Duration::from_secs(30);
        let left = loop {
            let left: Vec<i32> = mine.iter().copied().filter(|pid| alive(*pid) && std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !String::from_utf8_lossy(&c).replace('\0', " ").contains("endeavor connect"))).collect();
            if left.is_empty() || Instant::now() > deadline {
                break left;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        let said: Vec<String> = left.iter().map(|pid| format!("{pid}: {}", std::fs::read(format!("/proc/{pid}/cmdline")).map(|c| String::from_utf8_lossy(&c).replace('\0', " ")).unwrap_or_default())).collect();
        assert!(left.is_empty(), "still running after the job was cancelled: {said:?}");
        assert_eq!(found(&three), RuntimeState::NotRunning);
        for file in ["runtime.json", "job.json"] {
            assert!(!state.join(file).exists(), "{file} is removed once the job is over");
        }
        three.channel.detach();
        assert!(three.channel.closed().is_none());
        assert!(endeavor_jobs().iter().all(|id| before.contains(id)), "no job of this test is listed: {:?}", endeavor_jobs());
    });
    eprintln!("[{:?}] done", began.elapsed());
}
