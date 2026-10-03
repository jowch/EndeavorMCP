use std::net::TcpListener;

use super::*;
use crate::http;

fn env() -> Env {
    Env {
        home: PathBuf::from("/home/ada"),
        state_home: None,
        cache_home: None,
        scratch: None,
        cwd: PathBuf::from("/home/ada/project"),
        node: "lab3".into(),
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
            folder: PathBuf::from("/home/ada/project"),
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
    assert_eq!((o.port, o.folder, o.host_tools, o.idle_hours), (8456, PathBuf::from("/home/ada/project/data"), true, 0.0));
    assert_eq!((o.julia, o.depot.as_str(), o.state_dir), (julia::Source::Path("/opt/julia/bin/julia".into()), "/d:", PathBuf::from("/s")));
    let shell = parse(&["mcp", "--julia-shell", "module load julia", "--skills", "plugin", "--folder", "/abs"].map(String::from), &env());
    let Ok(Command::Mcp(o)) = shell else { panic!() };
    assert_eq!((o.julia, o.skills_plugin, o.folder), (julia::Source::Shell("module load julia".into()), true, PathBuf::from("/abs")));
    assert_eq!(parsed("stop --state-dir /s").unwrap(), Command::Stop { state_dir: PathBuf::from("/s") });
}

#[test]
fn flags_that_dont_apply_are_refused() {
    let error = |line: &str| parsed(line).unwrap_err();
    assert_eq!(error("mcp --host-tools"), "--host-tools isn't an option of mcp");
    assert_eq!(error("serve --skills plugin"), "--skills isn't an option of serve");
    assert_eq!(error("stop --port 1"), "--port isn't an option of stop");
    assert_eq!(error("mcp --skills all"), "--skills takes `plugin`, not all");
    assert_eq!(error("serve --idle-stop -1"), "--idle-stop needs a number of hours (0: never)");
    assert_eq!(error("serve --port http"), "--port needs a port number");
    assert_eq!(error("serve --julia auto --julia-shell x"), "give one of --julia and --julia-shell");
    assert_eq!(error("serve --folder"), "--folder needs a value");
    assert_eq!(error("serve --detach"), "unknown argument --detach");
}

#[test]
fn connection_details_on_a_workstation() {
    let text = connection_text(&Connection { port: 8456, token: "t0k", node: "lab3", folder: "/home/ada/project", login: None });
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
fn on_a_compute_node_the_tunnel_jumps_through_the_login_node() {
    let text = connection_text(&Connection { port: 8456, token: "t0k", node: "n2cn0216", folder: "/u/ada", login: Some("login2") });
    let forward = "From another computer, forward the port first:
    ssh -J login2 -L 8456:localhost:8456 n2cn0216
(This is a cluster's compute node: the jump goes through the login node, login2; use the name you ssh to.)

Connect";
    assert!(text.contains(forward), "{text}");
    let one_machine = connection_text(&Connection { port: 8456, token: "t0k", node: "lab3", folder: "/u/ada", login: Some("lab3") });
    assert!(one_machine.contains("first:\n    ssh -L 8456:localhost:8456 lab3\n\nConnect"), "a job on the login node itself: {one_machine}");
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("endeavor-standalone-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_runtime_unpacks_once_per_version() {
    let cache = scratch("unpack");
    let files: &[(&str, &[u8])] = &[("runtime/boot.jl", b"boot"), ("runtime/EndeavorRuntime/src/A.jl", b"a")];
    let first = unpack(&cache, "0.1.0-aaaa", files).unwrap();
    assert_eq!(first, cache.join("0.1.0-aaaa/runtime"));
    assert_eq!(std::fs::read(first.join("EndeavorRuntime/src/A.jl")).unwrap(), b"a");
    // A second run leaves the folder as it is (Julia may have written to it).
    std::fs::write(first.join("boot.jl"), "changed").unwrap();
    assert_eq!(unpack(&cache, "0.1.0-aaaa", files).unwrap(), first);
    assert_eq!(std::fs::read_to_string(first.join("boot.jl")).unwrap(), "changed");
    // Another version gets its own folder, next to the first.
    let second = unpack(&cache, "0.1.0-bbbb", files).unwrap();
    assert_eq!(std::fs::read_to_string(second.join("boot.jl")).unwrap(), "boot");
    let mut names: Vec<_> = std::fs::read_dir(&cache).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["0.1.0-aaaa", "0.1.0-bbbb"]);
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
    assert!(embedded::VERSION.starts_with(concat!(env!("CARGO_PKG_VERSION"), "-")), "{}", embedded::VERSION);
    assert_eq!(runtime, cache.join(embedded::VERSION).join("runtime"));
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

/// A stand-in for the runtime's `/mcp`: answers `tools/call` "json" as JSON,
/// "sse" as an event stream (a progress notification, then the reply), and
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
                Some("sse") => {
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

fn ready_relay(port: u16, skills_plugin: bool) -> (Arc<Relay>, Out) {
    let Ok(Command::Mcp(mut options)) = parsed("mcp") else { panic!() };
    options.skills_plugin = skills_plugin;
    let out = Out::default();
    let relay = Arc::new(Relay::new(options, "stdio-7".into(), Box::new(out.clone())));
    *relay.status.lock().unwrap() = Status::Ready { port, token: "t0k".into() };
    (relay, out)
}

#[test]
fn the_relay_answers_the_handshake_itself_and_passes_the_rest_on() {
    let (port, seen) = fake_core();
    let (relay, out) = ready_relay(port, true);
    relay.handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#);
    relay.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let lines = out.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let init: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["instructions"], crate::guide::STANDALONE, "with the plugin's skills, only what differs without the app");
    assert!(seen.lock().unwrap().is_empty(), "the runtime saw neither");

    relay.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"json","arguments":{}}}"#);
    assert_eq!(out.lines()[1], r#"{"id":1,"jsonrpc":"2.0","result":{"content":[]}}"#);
    let seen = seen.lock().unwrap();
    let (head, body) = &seen[0];
    assert_eq!(body, r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"json","arguments":{}}}"#);
    let header = |name| head.header(name).unwrap_or("(none)");
    assert_eq!(
        [header("Authorization"), header("X-Endeavor-Session"), header("X-Endeavor-Skills"), header("MCP-Protocol-Version"), header("Accept")],
        ["Bearer t0k", "stdio-7", "plugin", "2025-03-26", "application/json, text/event-stream"]
    );
    assert_eq!(head.target(), "/mcp");
}

#[test]
fn the_relay_passes_on_an_event_stream_event_by_event() {
    let (port, seen) = fake_core();
    let (relay, out) = ready_relay(port, false);
    relay.handle(r#"{"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"name":"sse","arguments":{},"_meta":{"progressToken":"p"}}}"#);
    assert_eq!(
        out.lines(),
        [
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"p"}}"#,
            r#"{"id":"a","jsonrpc":"2.0","result":{"content":[{"text":"ran","type":"text"}]}}"#,
        ]
    );
    // The session id the runtime gave goes back with the next request.
    relay.handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"json","arguments":{}}}"#);
    assert_eq!(seen.lock().unwrap()[1].0.header("Mcp-Session-Id"), Some("s-1"));
    assert!(seen.lock().unwrap()[0].0.header("X-Endeavor-Skills").is_none(), "no plugin, no header");
}

#[test]
fn a_notification_is_passed_on_and_gets_no_answer() {
    let (port, seen) = fake_core();
    let (relay, out) = ready_relay(port, false);
    relay.handle(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#);
    assert_eq!(seen.lock().unwrap()[0].1, r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#);
    assert!(out.lines().is_empty());
    relay.handle("not json");
    relay.tell_folder(port, "t0k");
    let (head, body) = &seen.lock().unwrap()[1];
    assert_eq!(head.target(), "/endeavor/call");
    assert_eq!(body, r#"{"id":1,"jsonrpc":"2.0","method":"endeavor/set_session_folder","params":{"folder":"/home/ada/project","owner":"stdio-7"}}"#);
    assert_eq!(out.lines(), [r#"{"error":{"code":-32700,"message":"Parse error"},"id":null,"jsonrpc":"2.0"}"#]);
}

#[test]
fn without_the_plugin_the_handshake_points_to_the_guide() {
    let (relay, out) = ready_relay(1, false);
    relay.handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#);
    relay.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let lines: Vec<Value> = out.lines().iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["result"]["instructions"], format!("{}\n\n{}", crate::guide::INSTRUCTIONS, crate::guide::STANDALONE));
    assert_eq!(lines[1]["result"]["tools"][0]["name"], "notebook_guide");
    assert!(!lines[1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "run_shell"), "an agent on this machine has its own shell");
}
