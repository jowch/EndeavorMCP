use super::*;
use crate::link::{JobInfo, QueueInfo};
use crate::standalone::{Command, Env, Options, parse};

fn status(state: State) -> link::Status {
    link::Status { machine: "lab".into(), name: "lab".into(), state, step: None, error: None, hello: None, runtime: None, job: None, queue: None, nothing_running: false, needs_install: None, pid: 1, build: "b".into() }
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
    let env = Env { home: "/home/ada".into(), state_home: None, cache_home: None, scratch: None, cwd: "/home/ada/project".into(), node: "lab3".into() };
    let Ok(Command::Mcp(options)) = parse(&["mcp".to_owned()], &env) else { panic!() };
    options
}

#[test]
fn a_link_that_is_not_ready_says_what_state_it_is_in_and_what_to_do() {
    let mut connecting = status(State::Connecting);
    connecting.step = Some("Connecting to lab".into());
    let said = not_ready_message("lab", &connecting);
    assert!(said.starts_with("Endeavor is connecting to lab. Last step: Connecting to lab") && said.contains("`pluto_session_status`") && said.contains("`use_machine` again"), "{said}");

    let mut starting = status(State::Starting);
    starting.step = Some("Found Julia 1.12.0".into());
    let said = not_ready_message("lab", &starting);
    assert!(said.contains("Julia is starting on lab") && said.contains("Last step: Found Julia 1.12.0") && said.contains("`pluto_session_status`"), "{said}");

    let mut queued = status(State::Queued);
    queued.job = Some(JobInfo { id: "4242".into(), ..Default::default() });
    queued.queue = Some(QueueInfo { state: "PENDING".into(), reason: "Priority".into() });
    let said = not_ready_message("hpc", &queued);
    assert_eq!(said, "The Slurm job 4242 on hpc is waiting in the queue: other jobs are ahead of it. Tell the user, wait, and call `pluto_session_status` to follow it.");
    queued.queue = Some(QueueInfo { state: "RUNNING".into(), reason: "n123".into() });
    assert!(not_ready_message("hpc", &queued).contains("running on node n123, and Julia is starting there"));

    let mut failed = status(State::Failed);
    failed.error = Some("lab refused the sign-in.".into());
    let said = not_ready_message("lab", &failed);
    assert!(said.starts_with("Julia on lab isn't available: lab refused the sign-in.") && said.contains("`use_machine` with machine \"lab\""), "{said}");

    let said = not_ready_message("lab", &status(State::Connected));
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
    let mut queued = status(State::Queued);
    queued.step = Some("Submitted job 7".into());
    queued.job = Some(JobInfo { id: "7".into(), summary: Some("8 CPUs · 32 GB · 8 h".into()), ..Default::default() });
    queued.queue = Some(QueueInfo { state: "PENDING".into(), reason: "Resources".into() });
    let result = status_result("hpc", &queued, "waits");
    assert_eq!(
        result,
        json!({
            "machine": "hpc", "state": "queued", "ready": false, "message": "waits", "step": "Submitted job 7",
            "queue": { "state": "PENDING", "reason": "Resources", "reason_text": "it waits for the resources it asked for to be free" },
            "job": { "id": "7", "summary": "8 CPUs · 32 GB · 8 h" },
        })
    );
    let mut failed = status(State::Failed);
    failed.error = Some("boom".into());
    assert_eq!(status_result("lab", &failed, "m")["error"], "boom");
    assert!(status_result("lab", &status(State::Connected), "m").get("queue").is_none());
}

#[test]
fn a_job_says_when_it_ends() {
    let mut ready = status(State::Ready);
    let now = unix_now();
    ready.job = Some(JobInfo { id: "9".into(), summary: None, node: Some("n1".into()), ends_at: Some(now + 3 * 3600 + 30) });
    let job = job_json(&ready).unwrap();
    assert_eq!((job["id"].as_str(), job["node"].as_str(), job["ends_at"].as_u64(), job["ends_in_minutes"].as_u64()), (Some("9"), Some("n1"), Some(now + 3 * 3600 + 30), Some(180)));
    assert!(job_json(&status(State::Ready)).is_none());
}

#[test]
fn only_a_link_nothing_hangs_on_is_replaced() {
    for state in [State::Connecting, State::Connected, State::Failed] {
        assert!(replaceable(&status(state)), "{state:?}");
    }
    for state in [State::Starting, State::Queued, State::Ready] {
        assert!(!replaceable(&status(state)), "{state:?}");
    }
    let mut attached = status(State::Connected);
    attached.runtime = Some(crate::link::RuntimeInfo { port: 1, token: "t".into(), mcp_url: String::new(), page_url: String::new(), node: "n".into(), pid: 2, reattached: false, job: None });
    assert!(!replaceable(&attached));
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
fn another_session_counts_as_active_for_fifteen_minutes() {
    let listed = json!([
        { "path": "/a.jl", "other_sessions": [{ "client": "Claude Code on lab", "active_seconds_ago": 600 }, { "client": null, "active_seconds_ago": 5 }] },
        { "path": "/b.jl", "other_sessions": [{ "client": "old", "active_seconds_ago": 901 }, { "client": "never", "active_seconds_ago": null }] },
        { "path": "/c.jl" },
    ]);
    let recent = recent_sessions(&listed);
    assert_eq!(recent, vec![json!({ "client": null, "active_seconds_ago": 5, "notebook": "/a.jl" }), json!({ "client": "Claude Code on lab", "active_seconds_ago": 600, "notebook": "/a.jl" })]);
    let said = others_result("hpc", recent);
    assert_eq!(said["stopped"], false);
    let message = said["message"].as_str().unwrap();
    assert!(message.contains("an unnamed client (less than a minute ago) in /a.jl") && message.contains("Claude Code on lab (10 min ago) in /a.jl") && message.contains("force true"), "{message}");
    assert!(recent_sessions(&json!([])).is_empty());
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
    *relay.target.lock().unwrap() = Target::Machine(Machine::new(&server, None));
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
    let mut queued = status(State::Queued);
    queued.job = Some(JobInfo { id: "4242".into(), ..Default::default() });
    queued.queue = Some(QueueInfo { state: "PENDING".into(), reason: "Priority".into() });
    let said = waiting_result("hpc", &queued);
    let message = said["message"].as_str().unwrap();
    assert_eq!((said["stopped"].clone(), said["job"]["id"].clone(), said["state"].clone()), (json!(false), json!("4242"), json!("queued")));
    assert!(message.contains("the Slurm job 4242 on hpc is pending (other jobs are ahead of it)") && message.contains("can't see which other sessions are waiting") && message.contains("force true"), "{message}");
    let said = waiting_result("lab", &status(State::Starting));
    assert!(said["message"].as_str().unwrap().contains("Julia is starting on lab") && said["job"].is_null(), "{said}");
}

#[test]
fn a_route_keeps_the_key_it_was_taken_with_when_the_session_moves() {
    let relay = Arc::new(Relay::new(options(), "s".into(), Box::new(std::io::sink())));
    let (before, key_before) = relay.placed();
    assert!(matches!(before, Target::Local { stopped: false }));
    let server = Server { id: "lab".into(), name: "Lab".into(), ssh_host: "lab".into(), ..Default::default() };
    let (left, old, ended) = relay.switch(Target::Machine(Machine::new(&server, None)));
    assert!((matches!(left, Target::Local { .. }), old.as_str(), ended) == (true, key_before.as_str(), true));
    let (after, key_after) = relay.placed();
    assert!(matches!(&after, Target::Machine(m) if m.id == "lab"));
    assert_ne!(key_after, key_before, "a new key goes with the new target");
    let (_, again, ended) = relay.switch(Target::Machine(Machine::new(&server, Some("/work".into()))));
    assert_eq!((again.as_str(), ended), (key_after.as_str(), false), "the same machine keeps its key");
    assert_eq!(relay.session(), key_after);
    let (_, _, ended) = relay.switch(Target::Local { stopped: false });
    assert!(ended);
    assert_ne!(relay.session(), key_after);
    let local_key = relay.session();
    let (_, _, ended) = relay.switch(Target::Local { stopped: false });
    assert!(!ended && relay.session() == local_key, "this computer to this computer is the same runtime");
    *relay.target.lock().unwrap() = Target::Local { stopped: true };
    let (_, _, ended) = relay.switch(Target::Local { stopped: false });
    assert!(ended, "a runtime that was stopped starts a new session");
}

#[test]
fn a_tool_call_that_cannot_get_the_lock_in_time_changes_nothing() {
    let relay = Arc::new(Relay::new(options(), "s".into(), Box::new(std::io::sink())));
    let key = relay.session();
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
    assert!(matches!(relay.placed(), (Target::Local { stopped: false }, session) if session == key));
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
    let route = Route { port, token: "t".into(), session: "s".into(), host: None };
    let started = Instant::now();
    let said = relay.recent_others(&route, Duration::from_millis(400)).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(said.starts_with("Nothing was stopped: couldn't check who else is active there (the runtime didn't answer in time)") && said.contains("`force: true`"), "{said}");
    holder.join().unwrap();
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
    *relay.target.lock().unwrap() = Target::Machine(Machine::new(&server, None));
    let decorated: Value = serde_json::from_str(&relay.decorate(&message, Some("pluto_session_status"), reply)).unwrap();
    let fields: Value = serde_json::from_str(decorated["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((fields["machine"].as_str(), fields["browser_url"].as_str(), fields["job"].clone()), (Some("Lab"), Some("http://localhost:1/"), Value::Null));
}

#[test]
fn a_call_to_a_stopped_computer_says_to_call_use_machine() {
    let relay = Arc::new(Relay::new(options(), "s".into(), Box::new(std::io::sink())));
    *relay.target.lock().unwrap() = Target::Local { stopped: true };
    let Err(unready) = relay.route(true) else { panic!("a stopped runtime has no route") };
    assert!(unready.message.contains("stop_machine") && unready.message.contains("`use_machine` with machine \"local\""), "{}", unready.message);
}
