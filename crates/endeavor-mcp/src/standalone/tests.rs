use std::net::TcpListener;

use super::*;
use crate::http;

fn env() -> Env {
    Env {
        home: PathBuf::from("/home/ada"),
        cwd: PathBuf::from("/home/ada/project"),
        node: "lab3".into(),
        ..Env::default()
    }
}

fn parsed(line: &str) -> Result<Command, String> {
    parse(&line.split(' ').map(String::from).collect::<Vec<_>>(), &env())
}

#[cfg(unix)]
#[test]
fn serve_without_flags_uses_this_folder_and_per_host_state() {
    assert_eq!(
        parsed("serve").unwrap(),
        Command::Serve(Options {
            state_dir: PathBuf::from("/home/ada/.local/state/endeavor/serve/lab3"),
            cache: PathBuf::from("/home/ada/.cache/endeavor/serve"),
            julia: julia::Source::Auto,
            depot: "/home/ada/.cache/endeavor/depot:".into(),
            folder: Some(PathBuf::from("/home/ada/project")),
            port: 0,
            host_tools: false,
            idle_hours: 48.0,
            skills_plugin: false,
        })
    );
    let cluster = Env { scratch: Some("/scratch/ada".into()), state_home: Some("/xdg/state".into()), cache_home: Some("/xdg/cache".into()), ..env() };
    let Ok(Command::Mcp(options)) = parse(&["mcp".to_owned()], &cluster) else { panic!() };
    assert_eq!(
        (options.depot.as_str(), options.state_dir, options.cache),
        ("/scratch/ada/endeavor/depot:", PathBuf::from("/xdg/state/endeavor/serve/lab3"), PathBuf::from("/xdg/cache/endeavor/serve"))
    );
}

#[test]
fn flags_set_what_they_name() {
    let Ok(Command::Serve(o)) = parsed("serve --port 8456 --folder data --host-tools --idle-stop 0 --julia /opt/julia/bin/julia --depot /d: --state-dir /s") else {
        panic!()
    };
    assert_eq!((o.port, o.folder, o.host_tools, o.idle_hours), (8456, Some(PathBuf::from("/home/ada/project/data")), true, 0.0));
    assert_eq!((o.julia, o.depot.as_str(), o.state_dir), (julia::Source::Path("/opt/julia/bin/julia".into()), "/d:", PathBuf::from("/s")));
    let shell = parse(&["mcp", "--julia-shell", "module load julia", "--skills", "plugin", "--folder", "/abs"].map(String::from), &env());
    let Ok(Command::Mcp(o)) = shell else { panic!() };
    assert_eq!((o.julia, o.skills_plugin, o.folder), (julia::Source::Shell("module load julia".into()), true, Some(PathBuf::from("/abs"))));
    assert_eq!(parsed("stop --state-dir /s").unwrap(), Command::Stop { state_dir: PathBuf::from("/s"), force: false });
    assert_eq!(parsed("stop --force --state-dir /s").unwrap(), Command::Stop { state_dir: PathBuf::from("/s"), force: true });
    assert!(parsed("status --force").unwrap_err().contains("--force isn't an option of status"));
    assert_eq!(parsed("status --json --state-dir /s").unwrap(), Command::Status { state_dir: PathBuf::from("/s"), json: true });
    assert!(matches!(parsed("status").unwrap(), Command::Status { json: false, .. }));
}

#[test]
fn no_folder_leaves_the_session_without_a_project_folder() {
    let Ok(Command::Mcp(o)) = parsed("mcp --no-folder --skills plugin") else { panic!() };
    assert_eq!(o.folder, None, "not the current folder");
    let Ok(Command::Mcp(o)) = parsed("mcp --skills plugin") else { panic!() };
    assert_eq!(o.folder, Some(PathBuf::from("/home/ada/project")), "the default is unchanged");
    assert_eq!(parsed("mcp --no-folder --folder /abs").unwrap_err(), "give one of --folder and --no-folder");
    assert_eq!(parsed("mcp --folder /abs --no-folder").unwrap_err(), "give one of --folder and --no-folder");
    assert_eq!(parsed("serve --no-folder").unwrap_err(), "--no-folder isn't an option of serve");
    assert_eq!(parsed("stop --no-folder").unwrap_err(), "--no-folder isn't an option of stop");
    let options = |line: &str| match parsed(line) {
        Ok(Command::Mcp(o)) => o,
        other => panic!("{other:?}"),
    };
    let without = core_env(&options("mcp --no-folder"), false);
    assert!(without.contains(&("ENDEAVOR_NO_FOLDER", Some("1".to_owned()))) && without.contains(&("ENDEAVOR_FOLDER", None)), "an inherited folder is cleared: {without:?}");
    let with = core_env(&options("mcp --folder /abs"), false);
    assert!(with.contains(&("ENDEAVOR_FOLDER", Some("/abs".to_owned()))) && with.contains(&("ENDEAVOR_NO_FOLDER", None)), "an inherited lack of one is cleared: {with:?}");
}

#[test]
fn flags_that_dont_apply_are_refused() {
    let error = |line: &str| parsed(line).unwrap_err();
    assert_eq!(error("mcp --host-tools"), "--host-tools isn't an option of mcp");
    assert_eq!(error("serve --skills plugin"), "--skills isn't an option of serve");
    assert_eq!(error("stop --port 1"), "--port isn't an option of stop");
    assert_eq!(error("serve --json"), "--json isn't an option of serve");
    assert_eq!(error("status --port 1"), "--port isn't an option of status");
    assert_eq!(error("mcp --skills all"), "--skills takes `plugin`, not all");
    assert_eq!(error("serve --idle-stop -1"), "--idle-stop needs a number of hours (0: never)");
    assert_eq!(error("serve --port http"), "--port needs a port number");
    assert_eq!(error("serve --julia auto --julia-shell x"), "give one of --julia and --julia-shell");
    assert_eq!(error("serve --folder"), "--folder needs a value");
    assert_eq!(error("serve --detach"), "unknown argument --detach");
}

#[test]
fn connection_details_on_a_workstation() {
    let text = connection_text(&Connection { port: 8456, token: "t0k", node: "lab3", folder: "/home/ada/project", project: true, login: None });
    assert_eq!(
        text,
        r#"Endeavor's notebooks are running on lab3, port 8456. New notebooks go in /home/ada/project.

Open them in a browser:
    http://localhost:8456/?token=t0k

From another computer, forward the port first:
    ssh -L 8456:localhost:8456 lab3

Connect an agent over MCP (Streamable HTTP):
    URL:    http://localhost:8456/mcp
    Header: Authorization: Bearer t0k

Claude Code:
    claude mcp add --transport http endeavor http://localhost:8456/mcp --header "Authorization: Bearer t0k"

Codex (~/.codex/config.toml):
    [mcp_servers.endeavor]
    url = "http://localhost:8456/mcp"
    http_headers = { Authorization = "Bearer t0k" }

Gemini CLI (~/.gemini/settings.json):
    {"mcpServers":{"endeavor":{"headers":{"Authorization":"Bearer t0k"},"httpUrl":"http://localhost:8456/mcp"}}}

Other agents (JSON):
    {"mcpServers":{"endeavor":{"headers":{"Authorization":"Bearer t0k"},"type":"http","url":"http://localhost:8456/mcp"}}}

The token lets anyone who has it run code as you. Keep it to yourself.
"#
    );
}

#[test]
fn a_runtime_without_a_folder_says_so_where_the_folder_would_be() {
    let text = connection_text(&Connection { port: 8456, token: "t0k", node: "lab3", folder: "/home/ada", project: false, login: None });
    assert!(text.starts_with("Endeavor's notebooks are running on lab3, port 8456. This runtime has no project folder: new notebooks without a path go in /home/ada, and agents give absolute paths.\n\nOpen them"), "{text}");
}

#[test]
fn on_a_compute_node_the_tunnel_jumps_through_the_login_node() {
    let text = connection_text(&Connection { port: 8456, token: "t0k", node: "n2cn0216", folder: "/u/ada", project: true, login: Some("login2") });
    let forward = "From another computer, forward the port first:
    ssh -J login2 -L 8456:localhost:8456 n2cn0216
(This is a cluster's compute node: the jump goes through the login node, login2; use the name you ssh to.)

Connect";
    assert!(text.contains(forward), "{text}");
    let one_machine = connection_text(&Connection { port: 8456, token: "t0k", node: "lab3", folder: "/u/ada", project: true, login: Some("lab3") });
    assert!(one_machine.contains("first:\n    ssh -L 8456:localhost:8456 lab3\n\nConnect"), "a job on the login node itself: {one_machine}");
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("endeavor-standalone-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_runtime_from_another_build_is_named() {
    let dir = scratch("other-build");
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(other_build(&dir), None, "nothing recorded");
    let write = |state: Value| std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
    write(json!({ "pid": 1, "build": embedded::BUILD_VERSION }));
    assert_eq!(other_build(&dir), None, "this build");
    write(json!({ "pid": 1, "build": "0.1.0-0000000000000000" }));
    let message = other_build(&dir).unwrap();
    assert!(message.contains("(build 0.1.0-0000000000000000; this is build ") && message.contains("run `endeavor stop`, then start it again"), "{message}");
    write(json!({ "pid": 1 }));
    assert!(other_build(&dir).unwrap().contains("(an earlier build; this is build "));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_runtime_unpacks_once_per_version() {
    let cache = scratch("unpack");
    let files: &[(&str, &[u8])] = &[("runtime/boot.jl", b"boot"), ("runtime/EndeavorRuntime/src/A.jl", b"a")];
    let first = unpack(&cache, "0.1.0-aaaa", files).unwrap().join("runtime");
    assert_eq!(first, cache.join("0.1.0-aaaa/runtime"));
    assert_eq!(std::fs::read(first.join("EndeavorRuntime/src/A.jl")).unwrap(), b"a");
    // A second run leaves the folder as it is (Julia may have written to it).
    std::fs::write(first.join("boot.jl"), "changed").unwrap();
    assert_eq!(unpack(&cache, "0.1.0-aaaa", files).unwrap().join("runtime"), first);
    assert_eq!(std::fs::read_to_string(first.join("boot.jl")).unwrap(), "changed");
    // Another version gets its own folder, next to the first.
    let second = unpack(&cache, "0.1.0-bbbb", files).unwrap().join("runtime");
    assert_eq!(std::fs::read_to_string(second.join("boot.jl")).unwrap(), "boot");
    assert_eq!(names(&cache), ["0.1.0-aaaa", "0.1.0-bbbb"]);
    std::fs::remove_dir_all(&cache).unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

/// As if `version`'s folder was last unpacked or used two days ago.
fn last_used_two_days_ago(cache: &Path, version: &str) {
    let marker = std::fs::OpenOptions::new().write(true).open(cache.join(version).join("in-use")).unwrap();
    marker.set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 3600)).unwrap();
}

#[test]
fn a_new_version_removes_older_folders_no_runtime_uses() {
    let cache = scratch("cleanup");
    let files: &[(&str, &[u8])] = &[("runtime/boot.jl", b"boot")];
    unpack(&cache, "0.1.0-aaaa", files).unwrap();
    let running = lease(&unpack(&cache, "0.1.0-bbbb", files).unwrap()).expect("an unpacked folder can be leased");
    unpack(&cache, "0.1.0-cccc", files).unwrap();
    // Unpacked by a build from before leases.
    std::fs::create_dir_all(cache.join("0.0.9-legacy/runtime")).unwrap();
    assert!(lease(&cache.join("0.0.9-legacy")).is_none());
    last_used_two_days_ago(&cache, "0.1.0-aaaa");
    last_used_two_days_ago(&cache, "0.1.0-bbbb");

    unpack(&cache, "0.1.0-dddd", files).unwrap();
    assert_eq!(names(&cache), ["0.0.9-legacy", "0.1.0-bbbb", "0.1.0-cccc", "0.1.0-dddd"]);

    // The runtime from bbbb stopped.
    drop(running);
    unpack(&cache, "0.1.0-eeee", files).unwrap();
    // A child another test is starting holds a copy of the lease until it execs.
    for _ in 0..100 {
        if !cache.join("0.1.0-bbbb").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        remove_unused(&cache, "0.1.0-eeee");
    }
    assert_eq!(names(&cache), ["0.0.9-legacy", "0.1.0-cccc", "0.1.0-dddd", "0.1.0-eeee"]);
    std::fs::remove_dir_all(&cache).unwrap();
}

#[test]
fn unpacking_a_version_again_counts_as_using_it() {
    let cache = scratch("reuse");
    let files: &[(&str, &[u8])] = &[("plugin/skill.md", b"skill")];
    unpack(&cache, "0.1.0-aaaa", files).unwrap();
    last_used_two_days_ago(&cache, "0.1.0-aaaa");
    unpack(&cache, "0.1.0-aaaa", files).unwrap();
    unpack(&cache, "0.1.0-bbbb", files).unwrap();
    assert_eq!(names(&cache), ["0.1.0-aaaa", "0.1.0-bbbb"]);
    std::fs::remove_dir_all(&cache).unwrap();
}

#[test]
fn the_binary_carries_runtime_folder() {
    let cache = scratch("embedded");
    let runtime = unpack_runtime(&cache).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
    for path in ["boot.jl", "Project.toml", "Manifest.toml", "EndeavorRuntime/src/EndeavorRuntime.jl"] {
        assert_eq!(std::fs::read(runtime.join(path)).unwrap(), std::fs::read(source.join(path)).unwrap(), "{path}");
    }
    assert!(embedded::RUNTIME_VERSION.starts_with(concat!(env!("CARGO_PKG_VERSION"), "-")), "{}", embedded::RUNTIME_VERSION);
    assert_eq!(runtime, cache.join(embedded::RUNTIME_VERSION).join("runtime"));
    std::fs::remove_dir_all(&cache).unwrap();
}

#[test]
fn the_binary_carries_plugin_folder() {
    let cache = scratch("plugin");
    let plugin = unpack(&cache, embedded::PLUGIN_VERSION, embedded::PLUGIN_FILES).unwrap().join("plugin");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
    for path in [".claude-plugin/plugin.json", "skills/endeavor-notebooks/SKILL.md", "skills/endeavor-notebooks/reference/pluto.md"] {
        assert_eq!(std::fs::read(plugin.join(path)).unwrap(), std::fs::read(source.join(path)).unwrap(), "{path}");
    }
    assert_ne!(embedded::PLUGIN_VERSION, embedded::RUNTIME_VERSION);
    std::fs::remove_dir_all(&cache).unwrap();
}

/// Lines a relay wrote to its stdout.
#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Out {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap().lines().map(str::to_owned).collect()
    }
}

/// A stand-in for the runtime's `/mcp`: answers `tools/call` `pluto_session_status` as JSON,
/// `list_notebooks` as an event stream (a progress notification, then the reply), and
/// a notification with `202`. What each request's head and body were.
fn fake_core() -> (u16, Arc<Mutex<Vec<(Head, String)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let mut client = client.unwrap();
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let head = Head::read(&mut reader).unwrap().unwrap();
            let body = String::from_utf8(http::read_body(&mut reader, head.request_body().unwrap()).unwrap()).unwrap();
            let message: Value = serde_json::from_str(&body).unwrap();
            log.lock().unwrap().push((head, body.clone()));
            let id = message["id"].clone();
            match message["params"]["name"].as_str() {
                _ if id.is_null() => http::respond(&mut client, "202 Accepted", None, b"", false).unwrap(),
                Some("list_notebooks") => {
                    write!(client, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nMcp-Session-Id: s-1\r\n\r\n").unwrap();
                    http::write_chunk(&mut client, b": waiting\n\n").unwrap();
                    let progress = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"p"}}"#;
                    http::write_chunk(&mut client, format!("event: message\ndata: {progress}\n\n").as_bytes()).unwrap();
                    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "ran" }] } });
                    http::write_chunk(&mut client, format!("event: message\ndata: {reply}\n\n").as_bytes()).unwrap();
                    http::write_chunk(&mut client, b"").unwrap();
                }
                _ => {
                    // Pretty-printed, as some servers send it: stdio needs it on one line.
                    let reply = serde_json::to_string_pretty(&json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [] } })).unwrap();
                    http::respond(&mut client, "200 OK", Some("application/json"), reply.as_bytes(), false).unwrap();
                }
            }
        }
    });
    (port, seen)
}

fn relay(skills_plugin: bool, state_dir: PathBuf) -> (Arc<Relay>, Out) {
    let Ok(Command::Mcp(mut options)) = parsed("mcp") else { panic!() };
    options.skills_plugin = skills_plugin;
    options.state_dir = state_dir;
    let out = Out::default();
    (Arc::new(Relay::new(options, "stdio-7".into(), Box::new(out.clone()))), out)
}

/// A relay on this computer whose runtime is the stand-in on `port`, found as a recorded one is.
fn ready_relay(port: u16, seen: &Mutex<Vec<(Head, String)>>, skills_plugin: bool) -> (Arc<Relay>, Out) {
    let dir = crate::client::scratch(&format!("relay-{port}"));
    // Windows takes a recorded process for alive only with the time it started.
    #[cfg(windows)]
    let started = crate::winproc::own_start_time();
    #[cfg(not(windows))]
    let started: Option<u64> = None;
    let record = json!({ "launcher": "process", "node": crate::hostname(), "pid": std::process::id(), "started": started, "token": "t0k", "port": port });
    std::fs::write(dir.join("runtime.json"), record.to_string()).unwrap();
    let (relay, out) = relay(skills_plugin, dir);
    match target::Provider::ensure(&*relay.local, crate::client::Want::Attach { install: false }, Duration::ZERO, false) {
        crate::client::Outcome::Ready(_) => {}
        crate::client::Outcome::Failed(why) => panic!("the recorded runtime wasn't found: {why}"),
        crate::client::Outcome::NothingRunning => panic!("the recorded runtime wasn't found: nothing running"),
        _ => panic!("the recorded runtime wasn't found"),
    }
    // The first call to a runtime tells it the session's folder.
    assert!(relay.route(machines::Need::Look, machines::Deadline::after(Duration::from_secs(5))).is_ok());
    seen.lock().unwrap().clear();
    (relay, out)
}

#[test]
fn the_relay_answers_the_handshake_itself_and_passes_the_rest_on() {
    let (port, seen) = fake_core();
    let (relay, out) = ready_relay(port, &seen, true);
    relay.handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"claude-code\u0007"}}}"#);
    relay.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let lines = out.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let init: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["instructions"], format!("{} {}", crate::guide::STANDALONE, crate::guide::MACHINES), "with the plugin's skills, what differs without the app, and the machine tools");
    assert!(seen.lock().unwrap().is_empty(), "the runtime saw neither");

    relay.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"pluto_session_status","arguments":{}}}"#);
    assert_eq!(out.lines()[1], r#"{"id":1,"jsonrpc":"2.0","result":{"content":[]}}"#);
    let seen = seen.lock().unwrap();
    let (head, body) = &seen[0];
    assert_eq!(body, r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"pluto_session_status","arguments":{}}}"#);
    let header = |name| head.header(name).unwrap_or("(none)");
    assert_eq!(
        [header("Authorization"), header("X-Endeavor-Session"), header("X-Endeavor-Skills"), header("MCP-Protocol-Version"), header("Accept")],
        ["Bearer t0k", "stdio-7", "plugin", "2025-03-26", "application/json, text/event-stream"]
    );
    assert_eq!(head.target(), "/mcp");
}

#[cfg(unix)]
#[test]
fn a_runtime_replaced_by_another_process_is_noticed_and_the_new_one_is_found() {
    let (port, seen) = fake_core();
    let (relay, _) = ready_relay(port, &seen, true);
    let before = target::Provider::status(&*relay.local).runtime.unwrap();
    assert_eq!(before.pid, std::process::id());
    // Another process has taken its place in the state folder: the parent of this test is one that is alive.
    let other = std::os::unix::process::parent_id();
    let record = json!({ "launcher": "process", "node": crate::hostname(), "pid": other, "started": null, "token": "other", "port": port });
    std::fs::write(relay.options.state_dir.join("runtime.json"), record.to_string()).unwrap();
    assert_eq!(target::Provider::status(&*relay.local).state, crate::client::State::Connected, "the one that was attached to is forgotten");
    let crate::client::Outcome::Ready(after) = target::Provider::ensure(&*relay.local, crate::client::Want::Attach { install: false }, Duration::ZERO, false) else { panic!("the new runtime wasn't found") };
    assert_eq!((after.pid, after.token.as_str()), (other, "other"));
}

#[test]
fn the_relay_passes_on_an_event_stream_event_by_event() {
    let (port, seen) = fake_core();
    let (relay, out) = ready_relay(port, &seen, false);
    relay.handle(r#"{"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"name":"list_notebooks","arguments":{},"_meta":{"progressToken":"p"}}}"#);
    assert_eq!(
        out.lines(),
        [
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"p"}}"#,
            r#"{"id":"a","jsonrpc":"2.0","result":{"content":[{"text":"ran","type":"text"}]}}"#,
        ]
    );
    // The session id the runtime gave goes back with the next request.
    relay.handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"pluto_session_status","arguments":{}}}"#);
    assert_eq!(seen.lock().unwrap()[1].0.header("Mcp-Session-Id"), Some("s-1"));
    assert!(seen.lock().unwrap()[0].0.header("X-Endeavor-Skills").is_none(), "no plugin, no header");
}

#[test]
fn a_call_with_the_wrong_arguments_is_answered_by_the_front_and_not_passed_on() {
    let (port, seen) = fake_core();
    let (relay, out) = ready_relay(port, &seen, false);
    let call = |id: u32, name: &str, arguments: &str| relay.handle(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#));
    call(1, "new_notebook", r#"{"name":"remote.jl"}"#);
    call(2, "edit_cell", r#"{"notebook_id":"n","cell_id":"c"}"#);
    call(3, "no_such_tool", "{}");
    relay.handle(r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"new_notebook","arguments":{"name":"x"}}}"#);
    assert!(seen.lock().unwrap().is_empty(), "the runtime saw none of them");
    let said: Vec<Value> = out.lines().iter().map(|l| serde_json::from_str::<Value>(l).unwrap()).map(|r| serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()).collect();
    assert_eq!(said.len(), 3, "a notification gets no answer: {said:?}");
    assert_eq!(said.iter().map(|s| s["error"].as_str().unwrap()).collect::<Vec<_>>(), ["invalid_argument", "invalid_argument", "unknown_tool"]);
    assert!(said[0]["message"].as_str().unwrap().starts_with("`name` is not an argument of `new_notebook`. Its arguments: `path`."), "{said:?}");
    call(4, "list_notebooks", r#"{"placeholder":""}"#);
    assert_eq!(seen.lock().unwrap().len(), 1, "a tool with no arguments ignores them and the call goes on");
}

#[test]
fn a_notification_is_passed_on_and_gets_no_answer() {
    let (port, seen) = fake_core();
    let (relay, out) = ready_relay(port, &seen, false);
    relay.handle(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#);
    assert_eq!(seen.lock().unwrap()[0].1, r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#);
    assert!(out.lines().is_empty());
    relay.handle("not json");
    relay.tell_session_folder(port, "t0k", relay.options.folder.as_ref().and_then(|folder| folder.to_str()));
    {
        let (head, body) = &seen.lock().unwrap()[1];
        assert_eq!(head.target(), "/endeavor/call");
        assert_eq!(body, r#"{"id":1,"jsonrpc":"2.0","method":"endeavor/set_session_folder","params":{"folder":"/home/ada/project","owner":"stdio-7"}}"#);
    }
    assert_eq!(out.lines(), [r#"{"error":{"code":-32700,"message":"Parse error"},"id":null,"jsonrpc":"2.0"}"#]);
    relay.tell_session_folder(port, "t0k", None);
    let (_, body) = &seen.lock().unwrap()[2];
    assert_eq!(body, r#"{"id":1,"jsonrpc":"2.0","method":"endeavor/set_session_folder","params":{"no_folder":true,"owner":"stdio-7"}}"#);
}

#[test]
fn without_the_plugin_the_handshake_points_to_the_guide() {
    let (relay, out) = relay(false, crate::client::scratch("relay-handshake"));
    relay.handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#);
    relay.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let lines: Vec<Value> = out.lines().iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    let instructions = lines[0]["result"]["instructions"].as_str().unwrap();
    assert!(instructions.starts_with(&format!("{} {} Before your first `add_machine`", crate::guide::STANDALONE, crate::guide::MACHINES)), "{instructions}");
    assert!(instructions.contains("\n\nBefore your first notebook tool call in a session, call `notebook_guide` once"), "{instructions}");
    assert_eq!(lines[1]["result"]["tools"][0]["name"], "notebook_guide");
    let names: Vec<&str> = lines[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for tool in ["run_shell", "read_file", "list_folder", "list_machines", "add_machine", "use_machine", "stop_machine"] {
        assert!(names.contains(&tool), "{tool} is listed: on this computer the host tools refuse, and after use_machine they work");
    }
    let read_only = |name: &str| lines[1]["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"].clone();
    assert_eq!([read_only("list_machines"), read_only("add_machine"), read_only("use_machine"), read_only("stop_machine")], [true, false, false, false]);
}
