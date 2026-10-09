use std::io::{BufReader, Write};

use super::*;
use crate::client::{JobInfo, QueueInfo};
use crate::standalone::{Command, Env, Options, parse};

fn status(state: State) -> Status {
    Status { machine: "lab".into(), name: "lab".into(), state, step: None, hello: None, job: None }
}

fn reached(outcome: Outcome, status: Status) -> Reached {
    Reached { outcome, status }
}

fn partition(name: &str, default: bool, minutes: Option<u32>, cpus: u32, mem_mb: u64) -> Partition {
    Partition { name: name.into(), default, max_minutes: minutes, cpus, mem_mb }
}

fn cluster() -> Cluster {
    Cluster {
        resources: Resources { partition: None, cpus: 8, mem_gb: 32, minutes: 480, gres: None, extra: Vec::new() },
        partitions: vec![partition("shared", true, Some(1440), 32, 128 * 1024), partition("gpu", false, Some(240), 16, 64 * 1024)],
        ..Default::default()
    }
}

fn options() -> Options {
    let env = Env { home: "/home/ada".into(), cwd: "/home/ada/project".into(), node: "lab3".into(), ..Env::default() };
    let Ok(Command::Mcp(options)) = parse(&["mcp".to_owned()], &env) else { panic!() };
    options
}

#[test]
fn a_machine_that_is_not_ready_says_what_state_it_is_in_and_what_to_do() {
    let mut connecting = status(State::Connecting);
    connecting.step = Some("Connecting to lab".into());
    let said = not_ready_message("lab", &reached(Outcome::StillWorking("Connecting to lab".into()), connecting));
    assert!(said.starts_with("Endeavor is connecting to lab. Last step: Connecting to lab") && said.contains("call the notebook tool you want again") && said.contains("don't call it repeatedly") && said.contains("`use_machine` again"), "{said}");

    let said = not_ready_message("lab", &reached(Outcome::StillWorking("Found Julia 1.12.0".into()), status(State::Starting { queue: None })));
    assert!(said.contains("Julia is starting on lab") && said.contains("Last step: Found Julia 1.12.0") && said.contains("each call waits up to 45 seconds") && !said.contains("how far it got"), "{said}");

    let job = Some(JobInfo { id: "4242".into(), ..Default::default() });
    let queued = Outcome::Queued { job: job.clone(), queue: QueueInfo { state: "PENDING".into(), reason: "Priority".into() } };
    let said = not_ready_message("hpc", &reached(queued, status(State::Queued(QueueInfo { state: "PENDING".into(), reason: "Priority".into() }))));
    assert_eq!(said, "The Slurm job 4242 on hpc is waiting in the queue: other jobs are ahead of it. Tell the user. To wait, call the notebook tool you want again: each call waits up to 45 seconds. `pluto_session_status` answers at once and only shows the job's state, so don't call it repeatedly. A queued job can wait minutes or hours: after a few tries, stop and let the user say when to check again.");
    let running = Outcome::Queued { job, queue: QueueInfo { state: "RUNNING".into(), reason: "n123".into() } };
    let said = not_ready_message("hpc", &reached(running, status(State::Starting { queue: Some(QueueInfo { state: "RUNNING".into(), reason: "n123".into() }) })));
    assert!(said.contains("running on node n123, and Julia is starting there") && said.contains("call the notebook tool you want again") && !said.contains("minutes or hours"), "{said}");

    let said = not_ready_message("lab", &reached(Outcome::Failed("lab refused the sign-in.".into()), status(State::Failed("lab refused the sign-in.".into()))));
    assert!(said.starts_with("Julia on lab isn't available: lab refused the sign-in.") && said.contains("`use_machine` with machine \"lab\""), "{said}");

    let said = not_ready_message("lab", &reached(Outcome::NothingRunning, status(State::NothingRunning)));
    assert!(said.contains("Julia isn't running on lab") && said.contains("`use_machine`"), "{said}");
}

#[test]
fn slurm_reasons_are_put_in_words() {
    assert_eq!(queue_reason_text("Resources"), "it waits for the resources it asked for to be free");
    assert_eq!(queue_reason_text("QOSMaxCpuPerUserLimit"), "a limit on the account or user applies (QOSMaxCpuPerUserLimit)");
    assert_eq!(queue_reason_text("None"), "no reason given yet");
    assert_eq!(queue_reason_text("SomethingNew"), "SomethingNew");
}

#[test]
fn a_status_result_has_what_the_agent_needs() {
    let queue = QueueInfo { state: "PENDING".into(), reason: "Resources".into() };
    let mut queued = status(State::Queued(queue.clone()));
    queued.step = Some("Submitted job 7".into());
    queued.job = Some(JobInfo { id: "7".into(), summary: Some("8 CPUs · 32 GB · 8 h".into()), ..Default::default() });
    let outcome = Outcome::Queued { job: queued.job.clone(), queue };
    let result = status_result("hpc", &reached(outcome, queued), "waits");
    assert_eq!(
        result,
        json!({
            "machine": "hpc", "state": "queued", "ready": false, "message": "waits", "step": "Submitted job 7",
            "queue": { "state": "PENDING", "reason": "Resources", "reason_text": "it waits for the resources it asked for to be free" },
            "job": { "id": "7", "summary": "8 CPUs · 32 GB · 8 h" },
        })
    );
    assert_eq!(status_result("lab", &reached(Outcome::Failed("boom".into()), status(State::Failed("boom".into()))), "m")["error"], "boom");
    let result = status_result("lab", &reached(Outcome::NothingRunning, status(State::NothingRunning)), "m");
    assert!(result.get("queue").is_none() && result["state"] == "connected");
    assert_eq!(status_result("lab", &reached(Outcome::StillWorking(String::new()), status(State::Starting { queue: None })), "m")["state"], "starting");
}

#[test]
fn a_job_says_when_it_ends() {
    let mut ready = status(State::Ready(attached_runtime()));
    let now = unix_now();
    ready.job = Some(JobInfo { id: "9".into(), summary: None, node: Some("n1".into()), ends_at: Some(now + 3 * 3600 + 30) });
    let job = job_json(&ready).unwrap();
    assert_eq!((job["id"].as_str(), job["node"].as_str(), job["ends_at"].as_u64(), job["ends_in_minutes"].as_u64()), (Some("9"), Some("n1"), Some(now + 3 * 3600 + 30), Some(180)));
    assert!(job_json(&status(State::Ready(attached_runtime()))).is_none());
}

#[test]
fn only_a_connection_nothing_hangs_on_is_replaced() {
    let needs = InstallInfo { items: Vec::new(), helper: None };
    for state in [State::Connecting, State::Connected, State::NothingRunning, State::Failed("no".into()), State::NeedsInstall(needs)] {
        assert!(replaceable(&status(state.clone())), "{state:?}");
    }
    let queue = QueueInfo { state: "PENDING".into(), reason: "Priority".into() };
    for state in [State::Starting { queue: None }, State::Queued(queue), State::Ready(attached_runtime())] {
        assert!(!replaceable(&status(state.clone())), "{state:?}");
    }
}

fn attached_runtime() -> RuntimeInfo {
    RuntimeInfo { port: 1, token: "t".into(), mcp_url: String::new(), page_url: String::new(), node: "n".into(), pid: 2, reattached: false, job: None, remote_port: None, build: None, interface: None }
}

#[test]
fn what_to_tell_about_the_page_after_the_session_depends_on_where_the_runtime_is() {
    let runtime = |remote_port| RuntimeInfo { port: 1, token: "t".into(), mcp_url: String::new(), page_url: String::new(), node: "n7".into(), pid: 2, reattached: false, job: None, remote_port, build: None, interface: None };
    let lab = Server { id: "lab".into(), ssh_host: "ada@lab".into(), ..Default::default() };
    let said = reach_text(&lab, &runtime(Some(41234)));
    assert!(said.contains("works while this session is connected") && said.contains("`ssh -L 41234:127.0.0.1:41234 ada@lab`"), "{said}");
    let said = reach_text(&Server { port: Some(2222), ..lab.clone() }, &runtime(Some(41234)));
    assert!(said.contains("`ssh -L 41234:127.0.0.1:41234 -p 2222 ada@lab`"), "{said}");
    let said = reach_text(&Server { cluster: Some(Cluster::default()), ..lab.clone() }, &runtime(Some(41234)));
    assert!(said.contains("node n7, port 41234") && !said.contains("ssh -L"), "no command is promised for a compute node: {said}");
    let said = reach_text(&lab, &runtime(None));
    assert!(said.contains("works while this session is connected") && !said.contains("ssh -L"), "an older helper gives no port: {said}");
}

#[test]
fn resources_given_go_over_the_saved_defaults_within_the_partitions_limits() {
    let args = json!({ "machine": "hpc", "cpus": 4, "hours": 1.5, "gpus": 2, "partition": "gpu", "account": "lab", "extra_sbatch_flags": ["--constraint=a100"] });
    let given = Given::parse(&args).unwrap();
    assert!(given.any());
    let (resources, account) = given.over(&cluster()).unwrap();
    assert_eq!((resources.partition.as_deref(), resources.cpus, resources.mem_gb, resources.minutes), (Some("gpu"), 4, 32, 90));
    assert_eq!((resources.gres.as_deref(), resources.extra.clone(), account.as_deref()), (Some("gpu:2"), vec!["--constraint=a100".to_owned()], Some("lab")));

    let (resources, _) = Given::parse(&json!({ "hours": 100, "cpus": 64, "memory_gb": 500, "partition": "gpu" })).unwrap().over(&cluster()).unwrap();
    assert_eq!((resources.minutes, resources.cpus, resources.mem_gb), (240, 16, 64), "kept to what the partition offers");

    assert!(!Given::parse(&json!({ "machine": "hpc", "folder": "/x" })).unwrap().any(), "a folder is not a resource");
    assert_eq!(Given::parse(&json!({ "gpus": "gpu:a100:1" })).unwrap().gres, Some(Some("gpu:a100:1".to_owned())));
}

#[test]
fn zero_gpus_clears_the_saved_gpus_and_counts_as_a_resource_given() {
    let mut with_gpus = cluster();
    with_gpus.resources.gres = Some("gpu:2".into());
    let none = Given::parse(&json!({ "gpus": 0 })).unwrap();
    assert_eq!(none.gres, Some(None));
    assert!(none.any(), "0 is given, so a cluster with no job is not asked about");
    let (resources, _) = none.over(&with_gpus).unwrap();
    assert_eq!(resources.gres, None, "it overrides the saved default");
    let (resources, _) = Given::parse(&json!({ "cpus": 2 })).unwrap().over(&with_gpus).unwrap();
    assert_eq!(resources.gres.as_deref(), Some("gpu:2"), "left out, the default stays");
    assert!(!resources.sbatch_args().iter().all(|a| !a.starts_with("--gres")));
}

#[test]
fn resources_that_make_no_sense_are_refused() {
    let said = |args: Value| Given::parse(&args).and_then(|g| g.over(&cluster()).map(|_| ())).unwrap_err();
    assert!(said(json!({ "partition": "nope" })).contains("There is no partition \"nope\" on this cluster. Its partitions: shared, gpu."));
    assert!(said(json!({ "hours": 0 })).contains("hours must be a number above 0"));
    assert!(said(json!({ "hours": "8" })).contains("hours must be a number above 0"));
    assert!(said(json!({ "cpus": 0 })).contains("cpus must be a whole number from 1 to 4096"));
    assert!(said(json!({ "cpus": "many" })).contains("cpus must be a whole number"));
    assert!(said(json!({ "gpus": "gpu 1" })).contains("gpus must be a count"));
    assert!(said(json!({ "extra_sbatch_flags": "--x" })).contains("must be a list of strings"));
    assert!(said(json!({ "extra_sbatch_flags": [3] })).contains("must be a list of strings"));
    assert!(said(json!({ "extra_sbatch_flags": ["--x\ny"] })).contains("line break or NUL"));
    assert!(said(json!({ "extra_sbatch_flags": ["--x\u{0}y"] })).contains("line break or NUL"));
    assert!(said(json!({ "extra_sbatch_flags": ["--x\ty"] })).contains("control characters"));
    assert!(said(json!({ "extra_sbatch_flags": ["--qos", "normal"] })).contains("\"normal\" doesn't start with \"-\""));
    assert!(said(json!({ "extra_sbatch_flags": ["--wrap=sleep 1"] })).contains("--wrap"));
    assert!(said(json!({ "extra_sbatch_flags": ["--wrap"] })).contains("--wrap"));
    assert!(said(json!({ "extra_sbatch_flags": ["--wra=x"] })).contains("--wrap"));
    assert!(said(json!({ "account": "a\nb" })).contains("account can't hold control characters"));
    assert!(said(json!({ "account": 3 })).contains("account must be a string"));
}

#[test]
fn a_machine_name_is_plain_and_not_local() {
    for name in ["hoffman2", "lab-1", "gpu_box.edu", "A1"] {
        assert!(valid_name(name).is_ok(), "{name}");
    }
    for name in ["", "-x", "x-", ".x", "a b", "a/b", "user@host", "a:b", &"x".repeat(65)] {
        assert!(valid_name(name).is_err(), "{name:?}");
    }
    let said = valid_name("Local").unwrap_err();
    assert!(said.contains("is this computer"), "{said}");
    assert_eq!(default_name("ada@gpu.example.edu"), "gpu.example.edu");
    assert_eq!(default_name("[::1]"), "1");
    assert_eq!(default_name("lab"), "lab");
}

#[test]
fn a_new_machine_gets_its_name_as_its_id_or_the_next_free_one() {
    let server = |id: &str| Server { id: id.into(), ..Default::default() };
    assert_eq!(new_id("Lab", &[]).unwrap(), "lab");
    assert_eq!(new_id("lab", &[server("server-1f"), server("lab")]).unwrap(), "lab-2");
    assert_eq!(new_id("lab", &[server("lab"), server("lab-2")]).unwrap(), "lab-3");
    assert!(new_id("nul", &[]).is_err(), "a name Windows keeps for devices can't be a folder");
}

#[test]
fn stopping_refuses_with_how_many_sessions_were_active_and_how_long_ago() {
    let said = others_result("hpc", &Others { count: 2, seconds_ago: 600 });
    assert_eq!((said["stopped"].clone(), said["active_sessions"].clone(), said["active_seconds_ago"].clone()), (json!(false), json!(2), json!(600)));
    let message = said["message"].as_str().unwrap();
    assert!(message.contains("2 other sessions were active on hpc in the last 15 minutes, the latest 10 min ago") && message.contains("force true"), "{message}");
    let message = others_result("hpc", &Others { count: 1, seconds_ago: 5 })["message"].as_str().unwrap().to_owned();
    assert!(message.contains("another session was active on hpc") && message.contains("the latest less than a minute ago"), "{message}");
}

#[test]
fn a_cluster_with_no_job_and_no_resources_gets_its_defaults_to_confirm_and_the_session_stays() {
    let relay = Relay::new(options(), "s".into(), Box::new(std::io::sink()));
    let said = relay.needs_job("hpc", &cluster());
    assert_eq!((said["needs_job"].clone(), said["state"].clone(), said["ready"].clone()), (json!(true), json!("needs_job"), json!(false)));
    assert_eq!(said["defaults"]["cpus"], 8);
    assert_eq!(said["defaults"]["hours"], 8.0);
    assert_eq!(said["defaults"]["summary"], "8 CPUs · 32 GB · 8 h");
    assert_eq!(said["partitions"][0]["name"], "shared");
    let message = said["message"].as_str().unwrap();
    assert!(message.contains("nothing was submitted") && message.contains("Ask the user to confirm") && message.contains("machine \"hpc\""), "{message}");
    assert!(message.contains("this session has not moved: it stays on this computer until `use_machine` is called"), "{message}");
    let server = Server { id: "lab".into(), name: "Lab".into(), ssh_host: "lab".into(), ..Default::default() };
    *relay.target.lock().unwrap() = Target::new(&server, None);
    assert!(relay.needs_job("hpc", &cluster())["message"].as_str().unwrap().contains("it stays on Lab until"));
}

#[test]
fn slurm_is_chosen_by_the_argument_else_by_what_was_saved_else_by_what_was_found() {
    let plain = Server { id: "lab".into(), ..Default::default() };
    let on_cluster = Server { cluster: Some(Cluster::default()), ..plain.clone() };
    assert_eq!(choose_mode(None, None, true), Ok(true), "a new machine with Slurm is a cluster");
    assert_eq!(choose_mode(None, None, false), Ok(false));
    assert_eq!(choose_mode(Some(false), None, true), Ok(false), "plain by choice, though Slurm is there");
    assert_eq!(choose_mode(Some(true), None, true), Ok(true));
    assert!(choose_mode(Some(true), None, false).unwrap_err().contains("found no Slurm"));
    assert!(choose_mode(Some(true), Some(&plain), false).is_err());
    assert_eq!(choose_mode(None, Some(&plain), true), Ok(false), "a machine saved as plain stays plain");
    assert_eq!(choose_mode(None, Some(&on_cluster), true), Ok(true));
    assert_eq!(choose_mode(None, Some(&on_cluster), false), Ok(true), "unchanged even when Slurm isn't found this time");
    assert_eq!(choose_mode(Some(true), Some(&plain), true), Ok(true), "changed by asking");
    assert_eq!(choose_mode(Some(false), Some(&on_cluster), true), Ok(false));
}

#[test]
fn stopping_without_force_a_start_that_is_under_way_names_what_would_be_cancelled() {
    let mut queued = status(State::Queued(QueueInfo { state: "PENDING".into(), reason: "Priority".into() }));
    queued.job = Some(JobInfo { id: "4242".into(), ..Default::default() });
    let said = waiting_result("hpc", &queued);
    let message = said["message"].as_str().unwrap();
    assert_eq!((said["stopped"].clone(), said["job"]["id"].clone(), said["state"].clone()), (json!(false), json!("4242"), json!("queued")));
    assert!(message.contains("the Slurm job 4242 on hpc is pending (other jobs are ahead of it)") && message.contains("can't see which other sessions are waiting") && message.contains("force true"), "{message}");
    let said = waiting_result("lab", &status(State::Starting { queue: None }));
    assert!(said["message"].as_str().unwrap().contains("Julia is starting on lab") && said["job"].is_null(), "{said}");
}

#[test]
fn a_call_that_waited_sees_that_the_session_moved_away_and_back() {
    let relay = Arc::new(Relay::new(options(), "s".into(), Box::new(std::io::sink())));
    let provider: Arc<dyn Provider> = relay.local.clone();
    let before = relay.current();
    assert!(relay.moved(&relay.current(), &before, &provider).is_none());
    let server = Server { id: "lab".into(), name: "Lab".into(), ssh_host: "lab".into(), ..Default::default() };
    relay.point_at(Target::new(&server, None));
    relay.point_at(Target::local(relay.options.folder.as_ref().map(|folder| folder.join("other")).as_deref()));
    let now = relay.current();
    assert_eq!((now.id.as_str(), now.moves), (before.id.as_str(), 2), "the same machine, another folder");
    let said = relay.moved(&now, &before, &provider).unwrap();
    assert!(said.message.contains("moved to another machine while the call waited"), "{}", said.message);
    relay.update_target(&now.id, |t| t.failed = true);
    assert_eq!(relay.current().moves, 2, "a change that is no move is not counted");
}

#[test]
fn a_tool_call_that_cannot_get_the_lock_in_time_changes_nothing() {
    let relay = Arc::new(Relay::new(options(), "s".into(), Box::new(std::io::sink())));
    let _busy = relay.ops.lock().unwrap();
    let started = Instant::now();
    let slow = Deadline::after(Duration::from_millis(300));
    for result in [
        relay.use_machine(&json!({ "machine": "local" }), slow),
        relay.stop_machine(&json!({ "machine": "local" }), slow),
        relay.add_machine(&json!({ "host": "lab" }), slow),
    ] {
        let said = result.unwrap_err();
        assert!(said.contains("Another machine tool call is still running") && said.contains("changed nothing"), "{said}");
    }
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let target = relay.current();
    assert!(target.is_local() && target.active);
}

#[test]
fn a_runtime_that_never_answers_the_check_for_other_sessions_is_given_up_on() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let holder = std::thread::spawn(move || {
        let (socket, _) = silent.accept().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        drop(socket);
    });
    let relay = Relay::new(options(), "s".into(), Box::new(std::io::sink()));
    let started = Instant::now();
    let said = relay.recent_others(port, "t", Duration::from_millis(400)).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(said.starts_with("Nothing was stopped: couldn't check who else is active there (the runtime didn't answer in time)") && said.contains("`force: true`"), "{said}");
    holder.join().unwrap();
}

#[test]
fn the_check_for_other_sessions_is_not_made_when_no_time_is_left_and_ends_with_the_time_given() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let relay = Relay::new(options(), "s".into(), Box::new(std::io::sink()));
    let said = relay.recent_others(port, "t", Duration::ZERO).unwrap_err();
    assert!(said.contains("no time was left to ask") && said.contains("`force: true`"), "{said}");
    assert!(silent.set_nonblocking(true).is_ok() && silent.accept().is_err(), "no connection was made");

    // A runtime that answers a little, slowly, is cut off by the time for the whole answer.
    silent.set_nonblocking(false).unwrap();
    let dripper = std::thread::spawn(move || {
        let (mut socket, _) = silent.accept().unwrap();
        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if socket.write_all(b"x").is_err() {
                break;
            }
        }
    });
    let started = Instant::now();
    let said = relay.recent_others(port, "t", Duration::from_millis(400)).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(said.contains("the runtime didn't answer in time"), "{said}");
    dripper.join().unwrap();
}

/// A runtime that answers `/endeavor/call` once with `status` and `body`.
fn answering(status: &str, body: &str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let reply = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let head = crate::http::Head::read(&mut reader).unwrap().unwrap();
        crate::http::read_body(&mut reader, head.request_body().unwrap()).unwrap();
        socket.write_all(reply.as_bytes()).unwrap();
    });
    port
}

#[test]
fn only_a_well_formed_answer_to_the_check_for_other_sessions_lets_a_stop_go_on() {
    let relay = Relay::new(options(), "s".into(), Box::new(std::io::sink()));
    let ask = |status: &str, body: &str| relay.recent_others(answering(status, body), "t", Duration::from_secs(5));
    let said = ask("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap_err();
    assert!(said.starts_with("Nothing was stopped: couldn't check who else is active there (200:") && said.contains("`force: true`"), "{said}");
    assert!(ask("200 OK", r#"{"result":{"count":"2"}}"#).is_err(), "a count that is no number");
    assert!(ask("200 OK", r#"{"result":{"count":1}}"#).is_err(), "someone active, and no time given");
    assert!(ask("500 Internal Server Error", r#"{"result":{"count":0,"active_seconds_ago":null}}"#).unwrap_err().contains("500:"), "a status that is not 200");
    assert!(ask("200 OK", r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#).unwrap_err().contains("Method not found"));
    assert!(ask("200 OK", "{}").is_err());
    let none = ask("200 OK", r#"{"result":{"count":0,"active_seconds_ago":null}}"#).unwrap();
    assert_eq!((none.count, none.seconds_ago), (0, 0));
    let some = ask("200 OK", r#"{"result":{"count":2,"active_seconds_ago":30}}"#).unwrap();
    assert_eq!((some.count, some.seconds_ago), (2, 30));
}

#[test]
fn a_notice_is_added_once_to_the_first_reply_that_answers_a_call() {
    let relay = Relay::new(options(), "s".into(), Box::new(std::io::sink()));
    *relay.notice.lock().unwrap() = Some("This project used lab, which is gone.".into());
    let message = json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "list_notebooks" } });
    let progress = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} }).to_string();
    assert_eq!(relay.decorate(&message, Some("list_notebooks"), progress.clone()), progress, "a progress notification isn't the answer");
    let other = json!({ "jsonrpc": "2.0", "id": 5, "result": { "content": [] } }).to_string();
    assert_eq!(relay.decorate(&message, Some("list_notebooks"), other.clone()), other, "nor is another request's answer");

    let reply = json!({ "jsonrpc": "2.0", "id": 4, "result": { "content": [{ "type": "text", "text": "[]" }], "isError": false } }).to_string();
    let decorated: Value = serde_json::from_str(&relay.decorate(&message, Some("list_notebooks"), reply.clone())).unwrap();
    assert_eq!(decorated["result"]["content"][0]["text"], "[]");
    assert_eq!(decorated["result"]["content"][1]["text"], "This project used lab, which is gone.");
    assert_eq!(relay.decorate(&message, Some("list_notebooks"), reply.clone()), reply, "once");

    *relay.notice.lock().unwrap() = Some("n".into());
    let error = json!({ "jsonrpc": "2.0", "id": 4, "error": { "code": -32603, "message": "no" } }).to_string();
    let decorated: Value = serde_json::from_str(&relay.decorate(&message, None, error)).unwrap();
    assert_eq!(decorated["error"]["message"], "no n");
}

#[test]
fn the_status_of_a_session_on_a_machine_names_the_machine() {
    let relay = Relay::new(options(), "s".into(), Box::new(std::io::sink()));
    let message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "pluto_session_status" } });
    let reply = json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": "{\"browser_url\":\"http://localhost:1/\",\"notebooks\":[]}" }], "isError": false } }).to_string();
    assert_eq!(relay.decorate(&message, Some("pluto_session_status"), reply.clone()), reply, "on this computer it is left as it is");
    let server = Server { id: "lab".into(), name: "Lab".into(), ssh_host: "lab".into(), ..Default::default() };
    *relay.target.lock().unwrap() = Target::new(&server, None);
    let decorated: Value = serde_json::from_str(&relay.decorate(&message, Some("pluto_session_status"), reply)).unwrap();
    let fields: Value = serde_json::from_str(decorated["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((fields["machine"].as_str(), fields["browser_url"].as_str(), fields["job"].clone()), (Some("Lab"), Some("http://localhost:1/"), Value::Null));
}

#[test]
fn a_call_to_a_stopped_computer_says_to_call_use_machine() {
    let relay = Arc::new(Relay::new(options(), "s".into(), Box::new(std::io::sink())));
    relay.target.lock().unwrap().active = false;
    let Err(unready) = relay.route(Need::Start, Deadline::after(Duration::from_secs(5))) else { panic!("a stopped runtime has no route") };
    assert!(unready.message.contains("stop_machine") && unready.message.contains("`use_machine` with machine \"local\""), "{}", unready.message);
}

fn needing(items: Vec<wire::Item>, helper: Option<crate::client::NeedsInstall>) -> InstallInfo {
    InstallInfo { items, helper }
}

#[test]
fn the_question_is_built_from_the_items_including_a_kind_it_has_never_seen() {
    let r = wire::Item { kind: "kernel".into(), name: "R 4.5.1".into(), size_mb: Some(120), place: Some("/home/ada/.cache/endeavor/r".into()) };
    let status = needing(vec![r.clone()], None);
    for tool in ["use_machine", "add_machine", "stop_machine"] {
        let said = install_text("lab", &status, tool);
        assert!(said.contains("Endeavor needs to install R 4.5.1 (about 120 MB, into /home/ada/.cache/endeavor/r) on lab."), "{said}");
        assert!(said.contains("Ask the user") && said.contains("`install: true`") && said.contains(tool), "{said}");
        assert!(!said.contains("helper program") && !said.contains("shell line"), "no sentence of a known kind: {said}");
    }
    let json = install_json(&status);
    assert_eq!(json["items"], json!([{ "kind": "kernel", "name": "R 4.5.1", "size_mb": 120, "place": "/home/ada/.cache/endeavor/r" }]));
    assert_eq!(json.get("os"), None, "no helper details for no helper");
    let result = needs_install_result("lab", &status, "use_machine");
    assert_eq!((result["state"].as_str(), result["needs_install"].clone(), result["ready"].clone()), (Some("needs_install"), json!(true), json!(false)));
    assert!(result["message"].as_str().unwrap().contains("R 4.5.1") && result["message"].as_str().unwrap().ends_with("Nothing was installed on lab."));

    let julia = wire::Item { kind: wire::KIND_RUNTIME.into(), name: "Julia 1.12.6".into(), size_mb: Some(289), place: None };
    let both = needing(vec![julia, r], None);
    let said = install_text("lab", &both, "use_machine");
    assert!(said.contains("Julia 1.12.6 wasn't found on lab. Endeavor can download its own copy (about 289 MB). Endeavor needs to install R 4.5.1 (about 120 MB, into /home/ada/.cache/endeavor/r) on lab."), "{said}");
    assert!(said.contains("`install: true` for this call only"), "{said}");
    assert!(said.contains("`module load julia`"), "the known kind adds its note: {said}");
}

#[test]
fn the_helper_item_keeps_what_was_found_on_the_machine() {
    let found = crate::client::NeedsInstall { os: "Linux".into(), arch: "x86_64".into(), folder: "/srv/e/abc".into(), bytes: Some(21_500_000), update: true, running: Some(Running::Process { pid: 77, checked: true }) };
    let status = InstallInfo::helper(found);
    let said = install_text("lab", &status, "stop_machine");
    for part in ["Endeavor's helper (about 22 MB, into /srv/e/abc)", "Linux x86_64", "Stopping the runtime there needs it", "this is an update", "process 77"] {
        assert!(said.contains(part), "{part}: {said}");
    }
    let json = install_json(&status);
    assert_eq!((json["update"].clone(), json["running"].clone(), json["os"].clone()), (json!(true), json!({ "process": 77 }), json!("Linux")));
}

#[test]
fn this_computer_is_called_this_computer_in_what_an_agent_reads() {
    let said = [
        not_ready_message("local", &reached(Outcome::StillWorking(String::new()), status(State::Starting { queue: None }))),
        not_ready_message("local", &reached(Outcome::NothingRunning, status(State::Connected))),
        not_ready_message("local", &reached(Outcome::Failed("no julia".into()), status(State::Failed("no julia".into())))),
        waiting_result("local", &status(State::Starting { queue: None }))["message"].as_str().unwrap().to_owned(),
        others_result("local", &Others { count: 1, seconds_ago: 5 })["message"].as_str().unwrap().to_owned(),
    ];
    for said in &said {
        assert!(said.contains("this computer") && !said.contains("on local"), "{said}");
    }
    assert!(said[1].contains("`use_machine` with machine \"local\""), "the argument is still local: {}", said[1]);
    let waiting = &said[3];
    assert!(waiting.contains("Julia is starting on this computer") && waiting.contains("Stopping cancels it") && waiting.contains("force true"), "a start there is cancelled by a forced stop: {waiting}");
    let on_machine = waiting_result("lab", &status(State::Starting { queue: None }))["message"].as_str().unwrap().to_owned();
    assert!(on_machine.contains("Stopping cancels it") && on_machine.contains("force true"), "{on_machine}");
    assert_eq!(waiting.replace("this computer", "lab"), on_machine, "this computer and a machine are told the same");
}

#[test]
fn a_failed_start_on_this_computer_goes_to_every_caller_and_only_a_call_that_asks_clears_it() {
    let dir = crate::client::scratch("local-failed");
    let mut options = options();
    options.state_dir = dir.join("state");
    options.cache = dir.join("cache");
    options.julia = crate::julia::Source::Path(dir.join("no-julia").display().to_string());
    let local = Arc::new(super::super::target::Local::new(options));
    let start = Want::Start { job: None, install: false };
    let waiters: Vec<_> = (0..2)
        .map(|_| {
            let (local, start) = (local.clone(), start.clone());
            std::thread::spawn(move || local.ensure(start, Duration::from_secs(60), false))
        })
        .collect();
    let outcomes: Vec<Outcome> = waiters.into_iter().map(|waiter| waiter.join().unwrap()).collect();
    let Outcome::Failed(why) = outcomes[0].clone() else { panic!("{:?}", outcomes[0]) };
    assert!(why.contains("no-julia") && outcomes[1] == Outcome::Failed(why.clone()), "both waiters are told, and the start stopped at looking for Julia: {outcomes:?}");
    assert_eq!(local.status().state, State::Failed(why.clone()), "a status call has it too");
    assert_eq!(local.ensure(Want::Attach { install: false }, Duration::ZERO, false), Outcome::Failed(why.clone()), "and a look doesn't use it up");
    assert_eq!(local.ensure(start.clone(), Duration::ZERO, false), Outcome::Failed(why), "nor does another start that doesn't ask");
    assert_eq!(local.ensure(Want::Attach { install: false }, Duration::ZERO, true), Outcome::NothingRunning, "a call that asks looks afresh");
    assert!(local.status().state.error().is_none());
}

/// A Julia that fails, and notes in `log` each time it is run.
#[cfg(unix)]
fn failing_julia(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let (script, log) = (dir.join("julia"), dir.join("runs"));
    std::fs::write(&script, format!("#!/bin/sh\necho run >> {}\nexit 1\n", log.display())).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    (script, log)
}

#[cfg(unix)]
#[test]
fn a_notebook_call_reports_a_failed_start_once_and_the_next_one_tries_again_and_queries_never_do() {
    let dir = crate::client::scratch("told-once");
    let (script, log) = failing_julia(&dir);
    let mut options = options();
    options.state_dir = dir.join("state");
    options.cache = dir.join("cache");
    options.julia = crate::julia::Source::Path(script.display().to_string());
    let relay = Relay::new(options, "s".into(), Box::new(std::io::sink()));
    let runs = || std::fs::read_to_string(&log).map_or(0, |text| text.lines().count());
    let route = |need| relay.route(need, Deadline::after(Duration::from_secs(60))).err().expect("no runtime comes up");
    let first = route(Need::Start);
    assert!(first.message.contains("isn't available") && first.message.contains("this computer"), "{}", first.message);
    let tried = runs();
    assert!(tried > 0 && relay.current().failed);

    let status = route(Need::Peek);
    let reached = status.reached.expect("the status tool says how it stands");
    assert!(matches!(reached.status.state, State::Failed(_)), "the failure comes with its error");
    assert!(route(Need::Look).message.contains("isn't available"));
    assert!(route(Need::Peek).message.contains("isn't available"), "a query doesn't use up the report");
    assert_eq!(runs(), tried, "no query tried again");
    assert!(relay.current().failed);

    assert!(route(Need::Start).message.contains("isn't available"));
    assert!(runs() > tried, "the notebook call after the report tried again");
}

#[test]
fn a_session_is_marked_told_its_folder_only_when_the_runtime_took_it() {
    let relay = Arc::new(Relay::new(options(), "s".into(), Box::new(std::io::sink())));
    let info = |port| RuntimeInfo { port, token: "t".into(), mcp_url: String::new(), page_url: String::new(), node: "n".into(), pid: 7, reattached: false, job: None, remote_port: None, build: None, interface: None };
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    relay.ready(&relay.current(), &info(closed), None);
    assert_eq!(relay.current().told, None, "nobody answered, so the next call tells it again");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let heard = std::thread::spawn(move || {
        let mut client = listener.accept().unwrap().0;
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let head = crate::http::Head::read(&mut reader).unwrap().unwrap();
        let body = String::from_utf8(crate::http::read_body(&mut reader, head.request_body().unwrap()).unwrap()).unwrap();
        crate::http::respond(&mut client, "200 OK", Some("application/json"), b"{}", false).unwrap();
        body
    });
    relay.ready(&relay.current(), &info(port), None);
    assert_eq!(relay.current().told, Some(7));
    assert!(heard.join().unwrap().contains(r#""folder":"/home/ada/project""#));
}
