//! The agent's MCP connection, MCP over SSE: `GET /sse` opens a session's
//! stream, and each `POST /message?sessionId=…` carries one JSON-RPC message,
//! whose reply goes out on that stream. Each agent session's messages carry
//! `X-Endeavor-Session` (its key) and, on a server, `X-Endeavor-Host`. What the
//! core doesn't answer itself goes to Julia's `/dispatch` with those headers,
//! and what the core keeps about notebooks (see `notebooks`) hears of each call.
//!
//! Each session has a run policy the app sets ("plan" | "ask" | "auto"). In
//! "plan" its notebook writes and runs are refused. "ask" and "auto" pass
//! through for now: runs are still gated by the app's Claude hook.

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::host_tools;
use crate::http::{self, Head};
use crate::notebooks::{Julia, Notebooks, Upstream};

const KEEPALIVE: Duration = Duration::from_secs(15);

/// Replies waiting for a session's stream; a full queue holds up the next POST.
const QUEUE: usize = 64;

/// Tools that change the notebook or run code, here or on the server.
pub const WRITE_TOOLS: [&str; 12] = [
    "edit_cell", "edit_cells", "add_cell", "delete_cell", "move_cell", "fold_cell", "new_notebook",
    "execute_cell", "submit_changes", "run_all_cells", "allow_execution", "run_shell",
];

/// What every client connection shares.
pub struct Bridge {
    pub julia: Arc<Julia>,
    pub notebooks: Arc<Notebooks>,
    pub token: String,
    sessions: Mutex<HashMap<String, SyncSender<String>>>,
    /// Each agent session's run policy, by its key.
    policies: Mutex<HashMap<String, String>>,
    /// Each agent session's working folder on this machine, by its key.
    folders: Mutex<HashMap<String, String>>,
    /// `run_shell`'s environment, changed from ours as Julia's was: its
    /// depot, and none of what the helper passes the runtime.
    shell_env: Vec<(&'static str, Option<String>)>,
}

/// Who sent a message: the agent session's key and the server it works on,
/// both empty for the app.
#[derive(Default)]
pub struct Caller {
    pub owner: String,
    pub host: String,
}

impl Caller {
    fn of(request: &Head) -> Caller {
        let header = |name| request.header(name).unwrap_or_default().to_owned();
        Caller { owner: header("X-Endeavor-Session"), host: header("X-Endeavor-Host") }
    }
}

impl Bridge {
    /// `depot` is Julia's JULIA_DEPOT_PATH.
    pub fn new(token: String, depot: &str) -> Bridge {
        let shell_env = vec![
            ("JULIA_DEPOT_PATH", Some(depot.to_owned())),
            ("ENDEAVOR_TOKEN", None),
            ("ENDEAVOR_STATE", None),
            ("ENDEAVOR_LAUNCHER", None),
        ];
        let julia = Arc::new(Julia::new(token.clone()));
        let clock = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        Bridge {
            notebooks: Arc::new(Notebooks::new(julia.clone(), Box::new(clock))),
            julia,
            token,
            sessions: Mutex::default(),
            policies: Mutex::default(),
            folders: Mutex::default(),
            shell_env,
        }
    }

    /// The reply to one of the app's `/call`s, if the core answers it; `None`
    /// passes it to Julia's `/call`. Only the app calls this route, so its
    /// tool calls have no caller.
    pub fn app_call(&self, raw: &[u8]) -> io::Result<Option<String>> {
        let Some(message) = serde_json::from_slice::<Value>(raw).ok().filter(Value::is_object) else {
            return Ok(None);
        };
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let text = |key: &str, default: &str| params.get(key).map_or(default.to_owned(), julia_string);
        match message["method"].as_str().unwrap_or_default() {
            "endeavor/set_policy" => {
                let (owner, policy) = (text("owner", ""), text("policy", "ask"));
                eprintln!("[ Info: Session {owner} policy: {policy}");
                self.policies.lock().unwrap().insert(owner, policy);
            }
            "endeavor/set_notebook" => {
                let (owner, notebook) = (text("owner", ""), params.get("notebook").filter(|n| !n.is_null()).map_or(String::new(), julia_string));
                self.notebooks.bind(&owner, &notebook);
                eprintln!("[ Info: Session {owner} notebook: {}", if notebook.is_empty() { "(none)" } else { &notebook });
            }
            "endeavor/set_idle_limit" => {
                let hours = match params.get("hours") {
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(48.0),
                    Some(Value::Bool(b)) => *b as u8 as f64,
                    _ => 48.0,
                };
                self.notebooks.set_idle_limit(hours);
                let shown = params.get("hours").map_or("48".into(), julia_string);
                eprintln!("[ Info: Idle notebooks stop after: {}", if hours == 0.0 { "never".into() } else { format!("{shown} hours") });
            }
            "endeavor/stop_notebook" => {
                let path = text("path", "");
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let reply = match self.notebooks.stop_notebook(&path) {
                    Ok(result) => {
                        eprintln!("[ Info: Stopped notebook {path}: {}", result["stopped"]);
                        json!({ "jsonrpc": "2.0", "id": id, "result": result })
                    }
                    Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e } }),
                };
                return Ok(Some(to_json(&reply)));
            }
            // Julia keeps it too, for new_notebook: its reply is the app's.
            "endeavor/set_session_folder" => {
                let (owner, folder) = (text("owner", ""), params.get("folder").filter(|f| !f.is_null()).map_or(String::new(), julia_string));
                let mut folders = self.folders.lock().unwrap();
                if folder.is_empty() {
                    folders.remove(&owner);
                } else {
                    folders.insert(owner, folder);
                }
                return Ok(None);
            }
            "tools/list" | "tools/call" => {
                return Ok(Some(self.dispatch(&message, raw, &Caller::default())?.unwrap_or_else(|| "{}".into())));
            }
            _ => return Ok(None),
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        Ok(Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": {} }))))
    }

    /// Serve `GET /sse`: a new session, and its stream until the client goes.
    pub fn stream(&self, request: &Head, mut client: TcpStream) -> io::Result<()> {
        let id = session_id()?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        self.sessions.lock().unwrap().insert(id.clone(), tx);
        let chunked = request.keeps_alive();
        let result = client
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n{}\r\n",
                    if chunked { "Transfer-Encoding: chunked\r\n" } else { "" }
                )
                .as_bytes(),
            )
            .and_then(|_| send_events(&mut client, chunked, &id, &rx, KEEPALIVE));
        self.sessions.lock().unwrap().remove(&id);
        result
    }

    /// Serve `POST /message?sessionId=…`: the reply goes out on the session's
    /// stream before this answers 202. Whether the connection can carry another request.
    pub fn post(&self, request: &Head, reader: &mut BufReader<TcpStream>, client: &mut TcpStream) -> io::Result<bool> {
        let body = http::read_body(reader, request.request_body()?)?;
        let keep_alive = request.keeps_alive();
        let id = query_param(request.target(), "sessionId").unwrap_or_default();
        let Some(session) = self.sessions.lock().unwrap().get(id).cloned() else {
            http::respond(client, "404 Not Found", None, br#"{"error":"Session not found"}"#, keep_alive)?;
            return Ok(keep_alive);
        };
        let Some(message) = serde_json::from_slice::<Value>(&body).ok().filter(Value::is_object) else {
            http::respond(client, "400 Bad Request", None, br#"{"error":"Invalid JSON"}"#, keep_alive)?;
            return Ok(keep_alive);
        };
        match self.dispatch(&message, &body, &Caller::of(request)) {
            Ok(reply) => {
                // A session whose stream has gone drops the reply.
                if let Some(reply) = reply {
                    let _ = session.send(reply);
                }
                http::respond(client, "202 Accepted", None, b"", keep_alive)?;
                Ok(keep_alive)
            }
            Err(_) => {
                let body = json!({ "error": "Julia's bridge isn't answering" }).to_string();
                http::respond(client, "502 Bad Gateway", Some("application/json"), body.as_bytes(), false)?;
                Ok(false)
            }
        }
    }

    /// The reply to one JSON-RPC message (`raw` is its text), if it gets one.
    fn dispatch(&self, message: &Value, raw: &[u8], caller: &Caller) -> io::Result<Option<String>> {
        // Notifications get no reply, and Julia had nothing to do for them.
        let id = match message.get("id") {
            None | Some(Value::Null) => return Ok(None),
            Some(id) => id,
        };
        let ok = |result: Value| Ok(Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))));
        match message["method"].as_str().unwrap_or_default() {
            "ping" => ok(json!({})),
            "tools/list" => {
                let mut reply: Value = serde_json::from_str(&self.ask_julia("/dispatch", raw, caller)?)?;
                if !caller.host.is_empty()
                    && let Some(tools) = reply["result"]["tools"].as_array_mut()
                {
                    tools.extend(host_tools::schemas());
                }
                for tool in reply["result"]["tools"].as_array_mut().into_iter().flatten() {
                    // MCP's read-only hint, what Claude Code's plan mode checks before prompting.
                    let read_only = !tool["name"].as_str().is_some_and(|name| WRITE_TOOLS.contains(&name));
                    tool["annotations"] = json!({ "readOnlyHint": read_only });
                }
                Ok(Some(to_json(&reply)))
            }
            "tools/call" => {
                let params = &message["params"];
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                // Julia reads a call's arguments before anything else, and fails
                // arguments that aren't an object its own way.
                let Some(name) = params["name"].as_str().filter(|_| arguments.is_object()) else {
                    let reply = self.ask_julia("/dispatch", raw, caller)?;
                    self.notebooks.publish();
                    return Ok(Some(reply));
                };
                self.notebooks.note_activity(&arguments);
                let result = if let Some(refusal) = self.refusal(caller, name) {
                    tool_error(&refusal)
                } else if host_tools::NAMES.contains(&name) {
                    let folder = self.folders.lock().unwrap().get(&caller.owner).cloned();
                    let env: Vec<_> = self.shell_env.iter().map(|(name, value)| (*name, value.as_deref())).collect();
                    match host_tools::call(name, &arguments, &host_tools::Shell { folder: folder.as_deref(), env: &env }) {
                        Ok(result) => json!({ "content": [{ "type": "text", "text": to_json(&result) }], "isError": false }),
                        Err(error) => tool_error(&error),
                    }
                } else if name == "keep_notebook_alive" {
                    match self.notebooks.keep_alive(&arguments) {
                        Ok(result) => json!({ "content": [{ "type": "text", "text": to_json(&result) }], "isError": false }),
                        Err(error) => tool_error(&error),
                    }
                } else if let Some(refusal) = self.notebooks.refusal(&caller.owner, name, &arguments) {
                    tool_error(&refusal)
                } else {
                    let reply = self.notebooks.forward_tool(&caller.owner, name, &arguments, || self.ask_julia("/dispatch", raw, caller));
                    self.notebooks.publish();
                    return reply.map(Some);
                };
                self.notebooks.publish();
                ok(result)
            }
            _ => self.ask_julia("/dispatch", raw, caller).map(Some),
        }
    }

    /// Why a session may not call `tool`, as the error Julia raised for it.
    fn refusal(&self, caller: &Caller, tool: &str) -> Option<String> {
        if host_tools::NAMES.contains(&tool) && caller.host.is_empty() {
            return Some(format!(
                "ArgumentError: host_tools::`{tool}` is only for sessions on a server. This session runs on this Mac: use your own file and shell tools."
            ));
        }
        let plan = self.policies.lock().unwrap().get(&caller.owner).is_some_and(|p| p == "plan");
        if plan && WRITE_TOOLS.contains(&tool) {
            let what = if tool == "run_shell" { "run a command on the server" } else { "change or run the notebook" };
            return Some(format!(
                "ArgumentError: plan_mode::Plan mode is read-only: `{tool}` would {what}. Finish the plan; the user switches modes to carry it out."
            ));
        }
        None
    }

    fn ask_julia(&self, path: &str, raw: &[u8], caller: &Caller) -> io::Result<String> {
        self.julia.ask(path, raw, caller)
    }
}

/// A failed tool call's result, from the text of the error Julia raised:
/// `ArgumentError: kind::message` names its kind; anything else is a `tool_error`.
pub(crate) fn tool_error(raw: &str) -> Value {
    let (kind, message) = match raw.split_once("::") {
        Some((kind, message)) => {
            let kind = kind.trim();
            (kind.rsplit(':').next().unwrap_or_default().trim(), message)
        }
        None => ("tool_error", raw),
    };
    let text = to_json(&json!({ "error": kind, "message": message }));
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// A JSON value as Julia's `string` shows it.
pub fn julia_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "nothing".into(),
        other => other.to_string(),
    }
}

/// Write a session's events: where to post, then each reply as it comes, with
/// a comment line after `keepalive` of quiet so proxies keep the stream open.
/// Returns when a write fails: the client has gone.
fn send_events(out: &mut impl Write, chunked: bool, id: &str, replies: &Receiver<String>, keepalive: Duration) -> io::Result<()> {
    let mut event = |text: String| -> io::Result<()> {
        if chunked {
            write!(out, "{:x}\r\n{text}\r\n", text.len())?;
        } else {
            out.write_all(text.as_bytes())?;
        }
        out.flush()
    };
    event(format!("event: endpoint\ndata: /message?sessionId={id}\n\n"))?;
    loop {
        match replies.recv_timeout(keepalive) {
            Ok(reply) => event(format!("event: message\ndata: {reply}\n\n"))?,
            Err(RecvTimeoutError::Timeout) => event(": keepalive\n\n".into())?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// A random (version 4) UUID.
fn session_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// `name`'s value in a target's query, as given (not percent-decoded).
fn query_param<'a>(target: &'a str, name: &str) -> Option<&'a str> {
    let (_, query) = target.split_once('?')?;
    query.split('&').filter_map(|pair| pair.split_once('=')).find(|(key, _)| *key == name).map(|(_, value)| value)
}

/// JSON the way Julia's JSON.jl writes it, byte for byte: object keys sorted
/// by their bytes, and DEL escaped. serde_json's own key order depends on a
/// feature another crate in the workspace turns on.
pub fn to_json(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value);
    out
}

fn write_json(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_json(out, value);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Value::String(text) => write_string(out, text),
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push_str(&Value::from(text).to_string().replace('\x7f', "\\u007f"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_json_as_julia_does() {
        let value = json!({ "b": 1, "A": [true, null, 2.5], "a": { "z": "x\u{7f}\u{1}/\"é", "_": {} }, "aa": [] });
        assert_eq!(to_json(&value), r#"{"A":[true,null,2.5],"a":{"_":{},"z":"x\u007f\u0001/\"é"},"aa":[],"b":1}"#);
    }

    #[test]
    fn reads_errors_as_julia_did() {
        let text = |raw: &str| tool_error(raw)["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(text("ArgumentError: not_found::No folder at /x::y"), r#"{"error":"not_found","message":"No folder at /x::y"}"#);
        assert_eq!(text("SystemError: opening file \"/x\": Permission denied"), r#"{"error":"tool_error","message":"SystemError: opening file \"/x\": Permission denied"}"#);
        assert_eq!(text("IOError: readdir(\"/a::b\"): denied"), r#"{"error":"readdir(\"/a","message":"b\"): denied"}"#);
        assert_eq!(tool_error("x")["isError"], true);
    }

    #[test]
    fn finds_query_params() {
        assert_eq!(query_param("/message?sessionId=abc&x=1", "sessionId"), Some("abc"));
        assert_eq!(query_param("/message?x=1&sessionId=", "sessionId"), Some(""));
        assert_eq!(query_param("/message", "sessionId"), None);
    }

    #[test]
    fn session_ids_are_random_uuids() {
        let (a, b) = (session_id().unwrap(), session_id().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!((&a[14..15], a.matches('-').count()), ("4", 4));
    }

    /// A writer the test can read back while `send_events` still holds it.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn streams_the_endpoint_replies_and_keepalives() {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let out = Shared::default();
        let mut writer = out.clone();
        let stream = std::thread::spawn(move || send_events(&mut writer, true, "s1", &rx, Duration::from_millis(100)));
        tx.send(r#"{"id":1}"#.into()).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        drop(tx);
        stream.join().unwrap().unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let endpoint = "event: endpoint\ndata: /message?sessionId=s1\n\n";
        let reply = "event: message\ndata: {\"id\":1}\n\n";
        let expected = format!("{:x}\r\n{endpoint}\r\n{:x}\r\n{reply}\r\n", endpoint.len(), reply.len());
        assert!(text.starts_with(&expected), "{text:?}");
        let keepalives = text[expected.len()..].matches("d\r\n: keepalive\n\n\r\n").count();
        assert!(keepalives >= 1, "{text:?}");
    }

    #[test]
    fn a_failed_write_ends_the_stream() {
        struct Gone;
        impl Write for Gone {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (_tx, rx) = mpsc::sync_channel::<String>(1);
        assert!(send_events(&mut Gone, false, "s1", &rx, Duration::from_millis(10)).is_err());
    }
}
